use mbuild_core::{
    BuildContext, BuilderError, BuilderSpec, InputArity, InputSlot, ProducerInfo,
    ResolvedInputs, ResolvedObject, StagedBuildResult, TypedBuilder, fsutil,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

const KIND_SOURCE_TREE: &str = "source-tree";
const KIND_BUILD_SCRIPT: &str = "build-script";
const KIND_CONTAINER_IMAGE: &str = "container-image";
const OUTPUT_DIR_NAME: &str = "out";

#[derive(Debug)]
enum BinaryError {
    InvalidConfig(String),
    InputResolutionFailed(String),
    PodmanFailed(String),
    BuildFailed(String),
    FsFailed(String),
}

impl BinaryError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::InputResolutionFailed(message)
            | Self::PodmanFailed(message)
            | Self::BuildFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for BinaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type BResult<T> = Result<T, BinaryError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryConfig {
    kind: String,
    optimize: String,
}

struct ScriptExecution {
    script_host_path: PathBuf,
    source_input_name: String,
}

struct ContainerExecution {
    image_ref: String,
}

pub struct BinaryBuilder;

static BINARY_INPUTS: &[InputSlot] = &[
    InputSlot {
        name: "image",
        arity: InputArity::One,
        allowed_kinds: &[KIND_CONTAINER_IMAGE],
    },
    InputSlot {
        name: "script",
        arity: InputArity::One,
        allowed_kinds: &[KIND_BUILD_SCRIPT],
    },
    InputSlot {
        name: "sources",
        arity: InputArity::Many,
        allowed_kinds: &[KIND_SOURCE_TREE],
    },
];

static BINARY_SPEC: BuilderSpec = BuilderSpec {
    tag: "Binary",
    inputs: BINARY_INPUTS,
};

impl TypedBuilder for BinaryBuilder {
    type Config = BinaryConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &BINARY_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: ResolvedInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_config(&config).map_err(map_error)?;
        let image = inputs.one("image")?;
        let script = inputs.one("script")?;
        let sources = inputs.many("sources")?;
        if sources.is_empty() {
            return Err(BuilderError::ExecutionFailed(
                "Binary builder requires at least one source-tree input".to_string(),
            ));
        }

        let script_execution = resolve_script_execution(script, sources).map_err(map_error)?;
        let container_execution = resolve_container_execution(image).map_err(map_error)?;

        fsutil::recreate_empty_dir_force(&cx.temp_root)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let output_path = cx.temp_root.join(OUTPUT_DIR_NAME);
        fsutil::recreate_empty_dir(&output_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;

        let build_result = run_container_build(
            &container_execution,
            &script_execution,
            sources,
            &output_path,
            current_uid_gid(),
            &cx.builder_root,
        );

        if let Err(error) = build_result {
            let _ = fsutil::remove_dir_force(&cx.temp_root);
            return Err(map_error(error));
        }

        let mut attrs = Map::new();
        attrs.insert("optimize".to_string(), Value::String(config.optimize));
        attrs.insert(
            "install".to_string(),
            serde_json::json!({
                "owners": [
                    {
                        "path": "**",
                        "uid": 0,
                        "gid": 0,
                    }
                ]
            }),
        );

        let mut input_object_hashes = Vec::with_capacity(2 + sources.len());
        input_object_hashes.push(image.object_hash);
        input_object_hashes.push(script.object_hash);
        input_object_hashes.extend(sources.iter().map(|source| source.object_hash));

        Ok(StagedBuildResult {
            kind: config.kind,
            producer: ProducerInfo {
                builder: "binary".to_string(),
            },
            input_object_hashes,
            attrs,
            staged_path: output_path,
        })
    }
}

fn validate_config(config: &BinaryConfig) -> BResult<()> {
    if config.kind.trim().is_empty() {
        return Err(BinaryError::InvalidConfig(
            "kind must not be empty".to_string(),
        ));
    }
    if config.optimize.trim().is_empty() {
        return Err(BinaryError::InvalidConfig(
            "optimize must not be empty".to_string(),
        ));
    }
    Ok(())
}

fn resolve_script_execution(script: &ResolvedObject, sources: &[ResolvedObject]) -> BResult<ScriptExecution> {
    if !script.object_path.is_file() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "build-script input must resolve to a file: {}",
            script.object_path.display()
        )));
    }
    if let Some(source) = sources.iter().find(|source| !source.object_path.is_dir()) {
        return Err(BinaryError::InputResolutionFailed(format!(
            "source-tree input must resolve to a directory: {}",
            source.object_path.display()
        )));
    }

    Ok(ScriptExecution {
        script_host_path: script.object_path.clone(),
        source_input_name: "sources0".to_string(),
    })
}

fn resolve_container_execution(image: &ResolvedObject) -> BResult<ContainerExecution> {
    if !image.object_path.is_file() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "container-image input must resolve to a file: {}",
            image.object_path.display()
        )));
    }

    let image_ref = image
        .attrs
        .get("image_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            BinaryError::InputResolutionFailed(
                "container-image input does not define string attr 'image_ref'".to_string(),
            )
        })?;

    if image_ref.trim().is_empty() {
        return Err(BinaryError::InputResolutionFailed(
            "container-image input has empty attr 'image_ref'".to_string(),
        ));
    }

    Ok(ContainerExecution {
        image_ref: image_ref.to_string(),
    })
}

fn run_container_build(
    container: &ContainerExecution,
    script: &ScriptExecution,
    sources: &[ResolvedObject],
    output_path: &Path,
    (uid, gid): (u32, u32),
    builder_root: &Path,
) -> BResult<()> {
    let mut process = ProcessCommand::new("podman");
    process
        .arg("run")
        .arg("--rm")
        .arg("--network=none")
        .arg("--userns=keep-id")
        .arg("--user")
        .arg(format!("{}:{}", uid, gid));

    for (index, source) in sources.iter().enumerate() {
        process.arg("--volume").arg(format!(
            "{}:/in/sources{}:O",
            source.object_path.display(),
            index
        ));
    }

    process.arg("--volume").arg(format!(
        "{}:/out/{}:rw",
        output_path.display(),
        OUTPUT_DIR_NAME
    ));

    process.arg("--volume").arg(format!(
        "{}:/__mbuild_binary_script:ro",
        script.script_host_path.display()
    ));

    process
        .arg("--env")
        .arg(format!("MBUILD_SOURCE_INPUT={}", script.source_input_name))
        .arg("--env")
        .arg(format!("MBUILD_PRIMARY_OUTPUT={OUTPUT_DIR_NAME}"))
        .arg(&container.image_ref)
        .arg("/__mbuild_binary_script");

    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!("failed to execute podman run: {error}"))
    })?;
    let log_path = write_run_log(
        builder_root,
        &container.image_ref,
        &script.script_host_path,
        &script.source_input_name,
        &output,
    );

    if !output.status.success() {
        let log_hint = match &log_path {
            Some(path) => format!(" (log: {})", path.display()),
            None => String::new(),
        };
        return Err(BinaryError::BuildFailed(format!(
            "podman run failed with exit status {}: {}{}",
            output.status.code().unwrap_or(1),
            command_details(&output),
            log_hint,
        )));
    }

    if !output_path.is_dir() {
        return Err(BinaryError::BuildFailed(format!(
            "binary builder did not produce output directory '{}'",
            output_path.display()
        )));
    }

    Ok(())
}

fn write_run_log(
    builder_root: &Path,
    image_ref: &str,
    script_path: &Path,
    source_input_name: &str,
    output: &std::process::Output,
) -> Option<PathBuf> {
    let logs_dir = builder_root.join("logs");
    if let Err(error) = fs::create_dir_all(&logs_dir) {
        eprintln!(
            "warning: failed to create binary logs directory '{}': {}",
            logs_dir.display(),
            error
        );
        return None;
    }

    let timestamp = match fsutil::current_epoch_nanos() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("warning: failed to get current time for binary log file: {error}");
            return None;
        }
    };

    let run_log_path = logs_dir.join(format!("run-{timestamp}.log"));
    let log_content = format!(
        "image_ref: {}\nscript: {}\nsource_input: {}\nexit_code: {}\nstatus_success: {}\n\n=== stdout ===\n{}\n\n=== stderr ===\n{}\n",
        image_ref,
        script_path.display(),
        source_input_name,
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string()),
        output.status.success(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    if let Err(error) = fsutil::write_atomic(&run_log_path, &log_content) {
        eprintln!(
            "warning: failed to write binary run log '{}': {}",
            run_log_path.display(),
            error
        );
        return None;
    }

    Some(run_log_path)
}

fn command_details(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "command failed without output".to_string(),
        (false, true) => format!("stdout: {stdout}"),
        (true, false) => format!("stderr: {stderr}"),
        (false, false) => format!("stdout: {stdout}; stderr: {stderr}"),
    }
}

fn map_fsutil_error(error: fsutil::FsUtilError) -> BinaryError {
    BinaryError::FsFailed(error.to_string())
}

fn map_error(error: BinaryError) -> BuilderError {
    match error {
        BinaryError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        BinaryError::InputResolutionFailed(message)
        | BinaryError::PodmanFailed(message)
        | BinaryError::BuildFailed(message)
        | BinaryError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}

fn current_uid_gid() -> (u32, u32) {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        (uid, gid)
    }

    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{BuildKey, Builder, ObjectHash, ResolvedInputValue};
    use std::env;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn build_context(root: &Path) -> BuildContext {
        BuildContext {
            workspace_root: root.to_path_buf(),
            builder_root: root.join(".mbuild").join("builder-state").join("binary"),
            temp_root: root.join(".mbuild").join("builder-state").join("binary").join("tmp"),
        }
    }

    fn install_fake_podman(dir: &Path) {
        let script_path = dir.join("podman");
        fs::write(
            &script_path,
            r#"#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = run ]; then
  if [ "${MBUILD_TEST_BINARY_PODMAN_FAIL:-}" = "1" ]; then
    echo simulated podman run failure >&2
    exit 42
  fi
  shift 1
  source_input=""
  declare -A in_mounts
  out_host=""
  image_ref=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --volume)
        spec="$2"
        host="${spec%%:*}"
        rest="${spec#*:}"
        mount="${rest%%:*}"
        if [[ "$mount" == /in/* ]]; then
          name="${mount#/in/}"
          in_mounts["$name"]="$host"
        elif [ "$mount" = "/out/out" ]; then
          out_host="$host"
        fi
        shift 2
        ;;
      --env)
        kv="$2"
        case "$kv" in
          MBUILD_SOURCE_INPUT=*) source_input="${kv#*=}" ;;
        esac
        shift 2
        ;;
      --rm|--network=none|--userns=keep-id)
        shift 1
        ;;
      --user)
        shift 2
        ;;
      *)
        if [ -z "$image_ref" ]; then
          image_ref="$1"
        fi
        shift 1
        ;;
    esac
  done
  mkdir -p "$out_host/copied"
  cp -R "${in_mounts[$source_input]}/." "$out_host/copied/"
  printf '%s\n' "$image_ref" > "$out_host/image-ref.txt"
  exit 0
fi

echo unexpected podman invocation: "$@" >&2
exit 1
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
        }
    }

    fn with_fake_podman<T>(f: impl FnOnce() -> T) -> T {
        let _guard = env_lock().lock().unwrap();
        let temp = tempdir().unwrap();
        install_fake_podman(temp.path());
        let previous_path = env::var_os("PATH");
        let new_path = match &previous_path {
            Some(existing) => {
                let mut joined = temp.path().as_os_str().to_os_string();
                joined.push(":");
                joined.push(existing);
                joined
            }
            None => temp.path().as_os_str().to_os_string(),
        };
        unsafe { env::set_var("PATH", &new_path) };
        let result = f();
        match previous_path {
            Some(path) => unsafe { env::set_var("PATH", path) },
            None => unsafe { env::remove_var("PATH") },
        }
        result
    }

    fn resolved_object(root: &Path, kind: &str, name: &str, attrs: Map<String, Value>) -> ResolvedObject {
        let object_path = root.join(name);
        if kind == KIND_SOURCE_TREE {
            fs::create_dir_all(&object_path).unwrap();
            fs::write(object_path.join("README.txt"), b"hello source\n").unwrap();
        } else {
            fs::write(&object_path, b"payload\n").unwrap();
            #[cfg(unix)]
            if kind == KIND_BUILD_SCRIPT {
                let mut permissions = fs::metadata(&object_path).unwrap().permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&object_path, permissions).unwrap();
            }
        }
        ResolvedObject {
            object_hash: match kind {
                KIND_CONTAINER_IMAGE => "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                KIND_BUILD_SCRIPT => "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                _ => "sha256:3333333333333333333333333333333333333333333333333333333333333333",
            }
            .parse::<ObjectHash>()
            .unwrap(),
            build_key: match kind {
                KIND_CONTAINER_IMAGE => "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                KIND_BUILD_SCRIPT => "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                _ => "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            }
            .parse::<BuildKey>()
            .unwrap(),
            kind: kind.to_string(),
            attrs,
            object_path,
        }
    }

    fn sample_inputs(root: &Path) -> ResolvedInputs {
        let mut inputs = ResolvedInputs::empty();
        let mut image_attrs = Map::new();
        image_attrs.insert(
            "image_ref".to_string(),
            Value::String("docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
        );
        inputs.insert(
            "image",
            ResolvedInputValue::One(resolved_object(root, KIND_CONTAINER_IMAGE, "image.json", image_attrs)),
        );
        inputs.insert(
            "script",
            ResolvedInputValue::One(resolved_object(root, KIND_BUILD_SCRIPT, "script.sh", Map::new())),
        );
        inputs.insert(
            "sources",
            ResolvedInputValue::Many(vec![resolved_object(root, KIND_SOURCE_TREE, "src", Map::new())]),
        );
        inputs
    }

    #[test]
    fn binary_builder_runs_fake_podman_and_materializes_output_dir() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(
                    BinaryConfig {
                        kind: "binary-output".to_string(),
                        optimize: "size".to_string(),
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
                .unwrap();

            assert_eq!(result.kind, "binary-output");
            assert_eq!(result.producer.builder, "binary");
            assert_eq!(result.attrs["optimize"], Value::String("size".to_string()));
            assert_eq!(result.input_object_hashes.len(), 3);
            assert!(result.staged_path.is_dir());
            assert_eq!(
                fs::read_to_string(result.staged_path.join("copied").join("README.txt")).unwrap(),
                "hello source\n"
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("image-ref.txt")).unwrap(),
                "docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            );
        });
    }

    #[test]
    fn binary_builder_rejects_missing_image_ref_attr() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = ResolvedInputs::empty();
        inputs.insert(
            "image",
            ResolvedInputValue::One(resolved_object(temp.path(), KIND_CONTAINER_IMAGE, "image.json", Map::new())),
        );
        inputs.insert(
            "script",
            ResolvedInputValue::One(resolved_object(temp.path(), KIND_BUILD_SCRIPT, "script.sh", Map::new())),
        );
        inputs.insert(
            "sources",
            ResolvedInputValue::Many(vec![resolved_object(temp.path(), KIND_SOURCE_TREE, "src", Map::new())]),
        );

        let error = BinaryBuilder
            .build_typed(
                BinaryConfig {
                    kind: "binary-output".to_string(),
                    optimize: "size".to_string(),
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = BinaryBuilder
            .build_erased(
                serde_json::json!({
                    "kind": "binary-output",
                    "optimize": "size",
                    "extra": true,
                }),
                sample_inputs(temp.path()),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn binary_builder_reports_podman_run_failure() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            unsafe { env::set_var("MBUILD_TEST_BINARY_PODMAN_FAIL", "1") };
            let error = BinaryBuilder
                .build_typed(
                    BinaryConfig {
                        kind: "binary-output".to_string(),
                        optimize: "size".to_string(),
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
                .unwrap_err();
            unsafe { env::remove_var("MBUILD_TEST_BINARY_PODMAN_FAIL") };

            assert!(matches!(error, BuilderError::ExecutionFailed(_)));
            let message = error.to_string();
            assert!(message.contains("podman run"), "{message}");
        });
    }
}
