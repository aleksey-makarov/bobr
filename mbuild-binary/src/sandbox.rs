use crate::{
    BResult, BuildStep, CONFIG_DIR_NAME, OUTPUT_DIR_NAME, SandboxError, StepUser,
    collect_named_inputs, input_mount_path, map_error, map_fsutil_error, resolve_step_argv,
    resolve_step_cwd, resolve_step_env, validate_script_config, validate_step_interpolations,
    validate_steps, write_script_config,
};
use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    StagedBuildResult, TypedBuilder, fsutil,
};
use mbuild_runtime::{
    SandboxBuildConfig, SandboxInput, SandboxRunAs, SandboxStep, cached_host_idmap,
    run_sandbox_build,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct SandboxBuilder;
const FS_TREE_OBJECT_DIR_NAME: &str = "fs-tree-object";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub(crate) script_config: Option<Value>,
    pub(crate) steps: Vec<BuildStep>,
}

pub(crate) static SANDBOX_SPEC: BuilderSpec = BuilderSpec {
    tag: "Sandbox",
    required_inputs: &["rootfs"],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for SandboxBuilder {
    type Config = SandboxConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &SANDBOX_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_sandbox_config(&config).map_err(map_error)?;
        let rootfs = inputs.required("rootfs")?;
        validate_rootfs(rootfs).map_err(map_error)?;

        let named_inputs =
            collect_named_inputs(&SANDBOX_SPEC, "Sandbox", &inputs).map_err(map_error)?;
        validate_step_interpolations(&config.steps, &named_inputs).map_err(map_error)?;

        let output_path = cx.temp_dir.join(OUTPUT_DIR_NAME);
        fsutil::recreate_empty_dir_force(&output_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let config_path = cx.temp_dir.join(CONFIG_DIR_NAME);
        fsutil::recreate_empty_dir_force(&config_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        write_script_config(&config_path, config.script_config.as_ref()).map_err(map_error)?;

        let sandbox_config = resolve_sandbox_config(
            rootfs,
            &named_inputs,
            &config.steps,
            &output_path,
            &config_path,
            cx,
        )
        .map_err(map_error)?;
        let idmap = cached_host_idmap().map_err(|error| {
            BuilderError::ExecutionFailed(format!("failed to load host idmap: {error}"))
        })?;

        cx.log_event(
            BuildLogLevel::Info,
            "sandbox-prepare",
            format!(
                "resolved readonly rootfs, {} input mount(s), build dir, and config dir",
                named_inputs.len()
            ),
        );

        let outcome = run_sandbox_build(sandbox_config, idmap.as_ref()).map_err(|error| {
            BuilderError::ExecutionFailed(format!("sandbox build failed: {error}"))
        })?;
        write_build_report(cx, &outcome);

        let staged_path =
            stage_fs_tree_output(cx, &output_path, &outcome.manifest).map_err(map_error)?;

        Ok(StagedBuildResult {
            staged_path,
            object_hash: Some(outcome.object_hash),
        })
    }
}

fn validate_sandbox_config(config: &SandboxConfig) -> BResult<()> {
    validate_script_config(config.script_config.as_ref())?;
    validate_steps(&config.steps)
}

fn validate_rootfs(rootfs: &BuilderInputObject) -> BResult<()> {
    let rootfs_path = object_payload_path(&rootfs.object_path);
    if !rootfs_path.is_dir() {
        return Err(SandboxError::InputResolutionFailed(format!(
            "rootfs input must resolve to a directory: {}",
            rootfs_path.display()
        )));
    }
    Ok(())
}

fn resolve_sandbox_config(
    rootfs: &BuilderInputObject,
    inputs: &[(String, BuilderInputObject)],
    steps: &[BuildStep],
    output_path: &Path,
    config_path: &Path,
    cx: &BuildContext,
) -> BResult<SandboxBuildConfig> {
    let sandbox_inputs = inputs
        .iter()
        .map(|(name, input)| {
            let host_path = object_payload_path(&input.object_path);
            if !host_path.is_dir() && !host_path.is_file() {
                return Err(SandboxError::InputResolutionFailed(format!(
                    "sandbox input must resolve to a file or directory: {}",
                    host_path.display()
                )));
            }
            Ok(SandboxInput {
                name: name.clone(),
                host_path,
                mount_path: PathBuf::from(input_mount_path(name)),
            })
        })
        .collect::<BResult<Vec<_>>>()?;

    let sandbox_steps = steps
        .iter()
        .map(|step| resolve_sandbox_step(step, inputs, cx))
        .collect::<BResult<Vec<_>>>()?;

    let workspace = cx.temp_dir.join("runtime");
    let state_dir = cx.state_dir.join("runtime");
    std::fs::create_dir_all(&workspace).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to create sandbox runtime workspace '{}': {error}",
            workspace.display()
        ))
    })?;
    std::fs::create_dir_all(&state_dir).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to create sandbox runtime state dir '{}': {error}",
            state_dir.display()
        ))
    })?;

    Ok(SandboxBuildConfig {
        rootfs: object_payload_path(&rootfs.object_path),
        out_dir: output_path.to_path_buf(),
        config_dir: config_path.to_path_buf(),
        workspace,
        state_dir,
        inputs: sandbox_inputs,
        steps: sandbox_steps,
    })
}

fn object_payload_path(object_path: &Path) -> PathBuf {
    let fs_tree_root = object_path.join("root");
    if object_path.join("manifest.jsonl").is_file() && fs_tree_root.is_dir() {
        fs_tree_root
    } else {
        object_path.to_path_buf()
    }
}

fn stage_fs_tree_output(
    cx: &BuildContext,
    output_path: &Path,
    manifest: &mbuild_core::FsTreeManifest,
) -> BResult<PathBuf> {
    let staged_path = cx.temp_dir.join(FS_TREE_OBJECT_DIR_NAME);
    fsutil::recreate_empty_dir_force(&staged_path).map_err(map_fsutil_error)?;
    let manifest_path = staged_path.join("manifest.jsonl");
    manifest.write_canonical(&manifest_path).map_err(|error| {
        SandboxError::BuildFailed(format!(
            "failed to write sandbox fs-tree manifest '{}': {error}",
            manifest_path.display()
        ))
    })?;
    let root_path = staged_path.join("root");
    std::fs::rename(output_path, &root_path).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to stage sandbox output '{}' -> '{}': {error}",
            output_path.display(),
            root_path.display()
        ))
    })?;
    Ok(staged_path)
}

fn resolve_sandbox_step(
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
    cx: &BuildContext,
) -> BResult<SandboxStep> {
    let cwd = PathBuf::from(resolve_step_cwd(step, inputs)?);
    let argv = resolve_step_argv(step, inputs)?;
    let env = resolve_step_env(step, inputs)?
        .into_iter()
        .collect::<HashMap<_, _>>();
    let logs = cx.temp_dir.join("step-logs");
    std::fs::create_dir_all(&logs).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to create sandbox step log directory '{}': {error}",
            logs.display()
        ))
    })?;
    let log_name = sanitize_log_name(&step.name);
    let stdout_path = allocate_step_log_path(
        cx,
        &format!("sandbox-step-{log_name}-stdout"),
        logs.join(format!("{log_name}.stdout")),
    )?;
    let stderr_path = allocate_step_log_path(
        cx,
        &format!("sandbox-step-{log_name}-stderr"),
        logs.join(format!("{log_name}.stderr")),
    )?;

    Ok(SandboxStep {
        name: step.name.clone(),
        run_as: match step.run_as {
            StepUser::BuildUser => SandboxRunAs::BuildUser,
            StepUser::Root => SandboxRunAs::Root,
        },
        cwd,
        argv,
        env,
        stdout_path,
        stderr_path,
    })
}

fn allocate_step_log_path(cx: &BuildContext, label: &str, fallback: PathBuf) -> BResult<PathBuf> {
    let path = match cx.allocate_raw_log_path(label) {
        Ok(path) => path,
        Err(_) => fallback,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            SandboxError::FsFailed(format!(
                "failed to create sandbox log directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    Ok(path)
}

fn sanitize_log_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn write_build_report(cx: &BuildContext, outcome: &mbuild_runtime::SandboxBuildOutcome) {
    let steps = outcome
        .steps
        .iter()
        .map(|step| {
            let mut object = Map::new();
            object.insert("name".to_string(), Value::String(step.name.clone()));
            object.insert("run_as".to_string(), Value::String(step.run_as.clone()));
            object.insert("exit_code".to_string(), Value::from(step.exit_code));
            object.insert(
                "duration_ms".to_string(),
                Value::from(step.duration_ms as u64),
            );
            object.insert(
                "stdout_path".to_string(),
                Value::String(step.stdout_path.display().to_string()),
            );
            object.insert(
                "stderr_path".to_string(),
                Value::String(step.stderr_path.display().to_string()),
            );
            Value::Object(object)
        })
        .collect::<Vec<_>>();
    let manifest_jsonl = outcome
        .manifest
        .to_canonical_bytes()
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let report = serde_json::json!({
        "object_hash": outcome.object_hash.to_string(),
        "manifest_jsonl": manifest_jsonl,
        "steps": steps,
    });
    if let Ok(text) = serde_json::to_string_pretty(&report) {
        let log_path = cx.write_raw_log("sandbox-result", &text);
        cx.log_event_with_details(
            BuildLogLevel::Info,
            "sandbox-result",
            format!("sandbox output hash {}", outcome.object_hash),
            Some(outcome.object_hash),
            log_path,
            Map::new(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{Builder, BuilderInputObject, BuilderInputs};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn sandbox_spec_uses_rootfs_required_input() {
        assert_eq!(SANDBOX_SPEC.tag, "Sandbox");
        assert_eq!(SANDBOX_SPEC.required_inputs, &["rootfs"]);
        assert!(SANDBOX_SPEC.allow_extra_inputs);
    }

    #[test]
    fn sandbox_builder_rejects_missing_rootfs() {
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.state_dir).unwrap();
        std::fs::create_dir_all(&cx.temp_dir).unwrap();

        let config = json!({
            "steps": [{
                "name": "build",
                "run_as": "build-user",
                "cwd": "/",
                "argv": ["true"]
            }]
        });

        let error = SandboxBuilder
            .build_erased(config, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("rootfs"));
    }

    #[test]
    fn sandbox_builder_rejects_install_config() {
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.state_dir).unwrap();
        std::fs::create_dir_all(&cx.temp_dir).unwrap();

        let config = json!({
            "steps": [{
                "name": "build",
                "run_as": "build-user",
                "cwd": "/",
                "argv": ["true"]
            }],
            "install": {
                "rules": [{
                    "path": "**",
                    "attrs": {
                        "uid": 0,
                        "gid": 0,
                        "directory_mode": 493,
                        "regular_file_mode": 420,
                        "executable_file_mode": 493,
                        "symlink_mode": 511
                    }
                }]
            }
        });

        let error = SandboxBuilder
            .build_erased(config, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("unknown field `install`"));
    }

    #[test]
    fn stage_fs_tree_output_writes_manifest_and_moves_raw_output_to_root() {
        let temp = tempdir().unwrap();
        let cx = BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.temp_dir).unwrap();
        let output = cx.temp_dir.join("out");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("file"), "contents").unwrap();
        let manifest = mbuild_core::FsTreeManifest::from_entries(vec![
            mbuild_core::FsTreeEntry::directory("", 0, 0, 0o755),
            mbuild_core::FsTreeEntry::file("file", 0, 0, 0o644),
        ])
        .unwrap();

        let staged = stage_fs_tree_output(&cx, &output, &manifest).unwrap();

        assert!(staged.join("manifest.jsonl").is_file());
        assert_eq!(
            mbuild_core::FsTreeManifest::read_canonical(&staged.join("manifest.jsonl")).unwrap(),
            manifest
        );
        assert_eq!(
            std::fs::read_to_string(staged.join("root").join("file")).unwrap(),
            "contents"
        );
        assert!(!output.exists());
    }

    #[test]
    fn resolve_sandbox_config_maps_extra_inputs() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let source = temp.path().join("source");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        for path in [&rootfs, &source, &out, &config] {
            std::fs::create_dir_all(path).unwrap();
        }
        let cx = BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.temp_dir).unwrap();
        let rootfs_input = BuilderInputObject {
            object_path: rootfs,
        };
        let inputs = vec![(
            "source".to_string(),
            BuilderInputObject {
                object_path: source.clone(),
            },
        )];
        let step = BuildStep {
            name: "build".to_string(),
            run_as: StepUser::BuildUser,
            cwd: "@{source}".to_string(),
            argv: vec!["sh".to_string(), "-c".to_string(), "true".to_string()],
            env: Map::new(),
        };

        let config =
            resolve_sandbox_config(&rootfs_input, &inputs, &[step], &out, &config, &cx).unwrap();

        assert_eq!(config.inputs.len(), 1);
        assert_eq!(
            config.inputs[0].mount_path,
            PathBuf::from("/__mbuild/inputs/source")
        );
        assert_eq!(
            config.steps[0].cwd,
            PathBuf::from("/__mbuild/inputs/source")
        );
    }

    #[test]
    fn resolve_sandbox_config_uses_fs_tree_payload_roots() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs-object");
        let rootfs_payload = rootfs.join("root");
        let source = temp.path().join("source-object");
        let source_payload = source.join("root");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        for path in [&rootfs_payload, &source_payload, &out, &config] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(rootfs.join("manifest.jsonl"), "").unwrap();
        std::fs::write(source.join("manifest.jsonl"), "").unwrap();
        let cx = BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.temp_dir).unwrap();
        let rootfs_input = BuilderInputObject {
            object_path: rootfs,
        };
        let inputs = vec![(
            "source".to_string(),
            BuilderInputObject {
                object_path: source,
            },
        )];
        let step = BuildStep {
            name: "build".to_string(),
            run_as: StepUser::BuildUser,
            cwd: "@{source}".to_string(),
            argv: vec!["sh".to_string(), "-c".to_string(), "true".to_string()],
            env: Map::new(),
        };

        let config =
            resolve_sandbox_config(&rootfs_input, &inputs, &[step], &out, &config, &cx).unwrap();

        assert_eq!(config.rootfs, rootfs_payload);
        assert_eq!(config.inputs[0].host_path, source_payload);
    }
}
