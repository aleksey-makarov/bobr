use crate::{
    BResult, BUILD_DIR_ENV_VAR, BUILD_DIR_MOUNT_PATH, BUILD_DIR_NAME, BinaryConfig, BinaryError,
    BuildStep, CONFIG_DIR_NAME, CONFIG_ENV_VAR, CONFIG_MOUNT_PATH, INPUT_MOUNT_ROOT,
    OUT_DIR_ENV_VAR, OUT_DIR_MOUNT_PATH, OUTPUT_DIR_NAME, STEP_NAME_ENV_VAR, StepUser,
    collect_named_inputs, command_failure, current_uid_gid, default_install_meta, input_mount_path,
    map_error, map_fsutil_error, resolve_step_argv, resolve_step_cwd, resolve_step_env,
    run_command_or_cancel, validate_config, validate_step_interpolations, write_command_log,
    write_script_config,
};
use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    StagedBuildResult, TypedBuilder, fsutil,
};
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

const OVERLAY_DIR_NAME: &str = "input-overlays";
const ROOTFS_OVERLAY_DIR_NAME: &str = "rootfs-overlay";
const BASELINE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const BASELINE_HOME: &str = "/";
const BASELINE_USER: &str = "mbuild";

pub struct ContainerBuilder;

pub(crate) static CONTAINER_SPEC: BuilderSpec = BuilderSpec {
    tag: "Container",
    required_inputs: &["rootfs"],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

struct ResolvedContainerInput {
    name: String,
    object: BuilderInputObject,
    overlay: Option<InputOverlay>,
}

struct InputOverlay {
    upper: PathBuf,
    work: PathBuf,
}

impl TypedBuilder for ContainerBuilder {
    type Config = BinaryConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &CONTAINER_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_config(&config).map_err(map_error)?;
        let rootfs = inputs.required("rootfs")?;
        validate_rootfs(rootfs).map_err(map_error)?;

        let named_inputs =
            collect_named_inputs(&CONTAINER_SPEC, "Container", &inputs).map_err(map_error)?;
        validate_step_interpolations(&config.steps, &named_inputs).map_err(map_error)?;

        let output_path = cx.temp_dir.join(OUTPUT_DIR_NAME);
        fsutil::recreate_empty_dir_force(&output_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let build_path = cx.temp_dir.join(BUILD_DIR_NAME);
        fsutil::recreate_empty_dir_force(&build_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let config_path = cx.temp_dir.join(CONFIG_DIR_NAME);
        fsutil::recreate_empty_dir_force(&config_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        write_script_config(&config_path, config.script_config.as_ref()).map_err(map_error)?;

        let rootfs_overlay = create_rootfs_overlay(cx).map_err(map_error)?;
        let resolved_inputs = resolve_container_inputs(cx, &named_inputs).map_err(map_error)?;
        cx.log_event(
            BuildLogLevel::Info,
            "prepare",
            format!(
                "resolved rootfs, {} input(s), overlays, and config dir",
                named_inputs.len()
            ),
        );

        run_steps(
            rootfs,
            &rootfs_overlay,
            &resolved_inputs,
            &config.steps,
            &named_inputs,
            &build_path,
            &output_path,
            &config_path,
            cx,
        )
        .map_err(map_error)?;
        cx.check_cancelled()?;

        if !output_path.is_dir() {
            return Err(map_error(BinaryError::BuildFailed(format!(
                "container builder did not produce output directory '{}'",
                output_path.display()
            ))));
        }

        let mut meta = Map::new();
        let install = config.install.unwrap_or_else(default_install_meta);
        meta.insert(
            "install".to_string(),
            serde_json::to_value(&install).map_err(|error| {
                map_error(BinaryError::BuildFailed(format!(
                    "failed to serialize install metadata: {error}"
                )))
            })?,
        );

        Ok(StagedBuildResult {
            meta,
            staged_path: output_path,
            object_hash: None,
        })
    }
}

fn validate_rootfs(rootfs: &BuilderInputObject) -> BResult<()> {
    if !rootfs.object_path.is_dir() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "rootfs input must resolve to a directory: {}",
            rootfs.object_path.display()
        )));
    }
    Ok(())
}

fn create_rootfs_overlay(cx: &BuildContext) -> BResult<InputOverlay> {
    let root = cx.temp_dir.join(ROOTFS_OVERLAY_DIR_NAME);
    let upper = root.join("upper");
    let work = root.join("work");
    fsutil::recreate_empty_dir_force(&upper).map_err(map_fsutil_error)?;
    fsutil::recreate_empty_dir_force(&work).map_err(map_fsutil_error)?;
    Ok(InputOverlay { upper, work })
}

fn resolve_container_inputs(
    cx: &BuildContext,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<Vec<ResolvedContainerInput>> {
    let overlay_root = cx.temp_dir.join(OVERLAY_DIR_NAME);
    fsutil::recreate_empty_dir_force(&overlay_root).map_err(map_fsutil_error)?;

    let mut resolved = Vec::new();
    for (name, object) in inputs {
        if object.object_path.is_dir() {
            let root = overlay_root.join(name);
            let upper = root.join("upper");
            let work = root.join("work");
            fsutil::recreate_empty_dir_force(&upper).map_err(map_fsutil_error)?;
            fsutil::recreate_empty_dir_force(&work).map_err(map_fsutil_error)?;
            resolved.push(ResolvedContainerInput {
                name: name.clone(),
                object: object.clone(),
                overlay: Some(InputOverlay { upper, work }),
            });
        } else if object.object_path.is_file() {
            resolved.push(ResolvedContainerInput {
                name: name.clone(),
                object: object.clone(),
                overlay: None,
            });
        } else {
            return Err(BinaryError::InputResolutionFailed(format!(
                "container input must resolve to a file or directory: {}",
                object.object_path.display()
            )));
        }
    }

    Ok(resolved)
}

fn run_steps(
    rootfs: &BuilderInputObject,
    rootfs_overlay: &InputOverlay,
    resolved_inputs: &[ResolvedContainerInput],
    steps: &[BuildStep],
    named_inputs: &[(String, BuilderInputObject)],
    build_path: &Path,
    output_path: &Path,
    config_path: &Path,
    cx: &BuildContext,
) -> BResult<()> {
    for step in steps {
        exec_step(
            rootfs,
            rootfs_overlay,
            resolved_inputs,
            step,
            named_inputs,
            build_path,
            output_path,
            config_path,
            cx,
        )?;
    }

    Ok(())
}

fn exec_step(
    rootfs: &BuilderInputObject,
    rootfs_overlay: &InputOverlay,
    resolved_inputs: &[ResolvedContainerInput],
    step: &BuildStep,
    named_inputs: &[(String, BuilderInputObject)],
    build_path: &Path,
    output_path: &Path,
    config_path: &Path,
    cx: &BuildContext,
) -> BResult<()> {
    let log_tag = format!("step-{}", step.name);
    let cwd = resolve_step_cwd(step, named_inputs)?;
    let argv = resolve_step_argv(step, named_inputs)?;
    let env = resolve_step_env(step, named_inputs)?;
    let (uid, gid) = step_uid_gid(&step.run_as);

    cx.log_event(
        BuildLogLevel::Info,
        &log_tag,
        format!("running '{}' in bwrap container", step.name),
    );

    let mut process = ProcessCommand::new("bwrap");
    process
        .arg("--unshare-user")
        .arg("--unshare-pid")
        .arg("--unshare-net")
        .arg("--overlay-src")
        .arg(&rootfs.object_path)
        .arg("--overlay")
        .arg(&rootfs_overlay.upper)
        .arg(&rootfs_overlay.work)
        .arg("/")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp")
        .arg("--dir")
        .arg("/__mbuild")
        .arg("--dir")
        .arg(INPUT_MOUNT_ROOT)
        .arg("--ro-bind")
        .arg(config_path)
        .arg(CONFIG_MOUNT_PATH)
        .arg("--bind")
        .arg(build_path)
        .arg(BUILD_DIR_MOUNT_PATH)
        .arg("--bind")
        .arg(output_path)
        .arg(OUT_DIR_MOUNT_PATH);

    for input in resolved_inputs {
        let mount_path = input_mount_path(&input.name);
        if let Some(overlay) = &input.overlay {
            process
                .arg("--overlay-src")
                .arg(&input.object.object_path)
                .arg("--overlay")
                .arg(&overlay.upper)
                .arg(&overlay.work)
                .arg(&mount_path);
        } else {
            process
                .arg("--ro-bind")
                .arg(&input.object.object_path)
                .arg(&mount_path);
        }
    }

    process
        .arg("--clearenv")
        .arg("--setenv")
        .arg("PATH")
        .arg(BASELINE_PATH)
        .arg("--setenv")
        .arg("HOME")
        .arg(BASELINE_HOME)
        .arg("--setenv")
        .arg("USER")
        .arg(BASELINE_USER)
        .arg("--setenv")
        .arg(CONFIG_ENV_VAR)
        .arg(CONFIG_MOUNT_PATH)
        .arg("--setenv")
        .arg(BUILD_DIR_ENV_VAR)
        .arg(BUILD_DIR_MOUNT_PATH)
        .arg("--setenv")
        .arg(OUT_DIR_ENV_VAR)
        .arg(OUT_DIR_MOUNT_PATH)
        .arg("--setenv")
        .arg(STEP_NAME_ENV_VAR)
        .arg(&step.name);

    for (key, value) in env {
        process.arg("--setenv").arg(key).arg(value);
    }

    process
        .arg("--uid")
        .arg(uid.to_string())
        .arg("--gid")
        .arg(gid.to_string())
        .arg("--chdir")
        .arg(&cwd)
        .arg("--");
    for arg in &argv {
        process.arg(arg);
    }

    let output = run_command_or_cancel(
        process,
        cx,
        &log_tag,
        &rootfs.object_path.display().to_string(),
        named_inputs,
    )
    .map_err(|error| match error {
        crate::RunCommandError::Io(error) => BinaryError::BuildFailed(format!(
            "failed to execute bwrap for step '{}': {error}",
            step.name
        )),
        crate::RunCommandError::Cancelled => {
            BinaryError::Cancelled("build cancelled by signal".to_string())
        }
    })?;
    let log_path = write_command_log(
        cx,
        &log_tag,
        &rootfs.object_path.display().to_string(),
        named_inputs,
        &output,
    );

    if !output.status.success() {
        return Err(command_failure(
            &log_tag,
            &format!("bwrap step '{}'", step.name),
            &output,
            log_path,
        ));
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        &log_tag,
        format!("step '{}' completed", step.name),
        None,
        log_path,
        Map::new(),
    );

    Ok(())
}

fn step_uid_gid(run_as: &StepUser) -> (u32, u32) {
    match run_as {
        StepUser::BuildUser => current_uid_gid(),
        StepUser::Root => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::BuilderInputs;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock()
    }

    fn build_context(root: &Path) -> BuildContext {
        let state_dir = root.join(".mbuild").join("builder-state").join("container");
        let temp_dir = state_dir.join("tmp");
        fs::create_dir_all(&state_dir).unwrap();
        mbuild_core::fsutil::recreate_empty_dir_force(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    fn install_fake_bwrap(dir: &Path) {
        let script_path = dir.join("bwrap");
        fs::write(
            &script_path,
            r#"#!/usr/bin/env bash
set -euo pipefail
state_root="${MBUILD_TEST_BWRAP_STATE_ROOT:-$(dirname "$0")/.fake-bwrap-state}"
mkdir -p "$state_root"
next_file="$state_root/next"
if [ -f "$next_file" ]; then
  idx="$(cat "$next_file")"
else
  idx=0
fi
echo "$((idx + 1))" > "$next_file"
call_dir="$state_root/call-$idx"
mkdir -p "$call_dir"
printf '%s\n' "$@" > "$call_dir/argv"

out_host=""
step_name=""
prev=""
bind_src=""
setenv_key=""
for arg in "$@"; do
  case "$prev" in
    bind-src)
      bind_src="$arg"
      prev="bind-dest"
      continue
      ;;
    bind-dest)
      if [ "$arg" = "/__mbuild/out" ]; then
        out_host="$bind_src"
      fi
      prev=""
      continue
      ;;
    setenv-key)
      setenv_key="$arg"
      prev="setenv-value"
      continue
      ;;
    setenv-value)
      if [ "$setenv_key" = "MBUILD_STEP_NAME" ]; then
        step_name="$arg"
      fi
      prev=""
      continue
      ;;
  esac

  case "$arg" in
    --bind)
      prev="bind-src"
      ;;
    --setenv)
      prev="setenv-key"
      ;;
  esac
done

if [ "${MBUILD_TEST_BWRAP_FAIL:-}" = "1" ]; then
  echo simulated bwrap failure >&2
  exit 42
fi

if [ -n "$out_host" ]; then
  mkdir -p "$out_host"
  printf '%s\n' "$step_name" >> "$out_host/steps.txt"
fi
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

    fn with_fake_bwrap<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempdir().unwrap();
        let state_root = temp.path().join(".fake-bwrap-state");
        install_fake_bwrap(temp.path());
        let previous_path = env::var_os("PATH");
        let previous_state = env::var_os("MBUILD_TEST_BWRAP_STATE_ROOT");
        let new_path = match &previous_path {
            Some(existing) => {
                let mut joined = temp.path().as_os_str().to_os_string();
                joined.push(":");
                joined.push(existing);
                joined
            }
            None => temp.path().as_os_str().to_os_string(),
        };
        unsafe {
            env::set_var("PATH", &new_path);
            env::set_var("MBUILD_TEST_BWRAP_STATE_ROOT", &state_root);
        }
        let result = f(&state_root);
        match previous_path {
            Some(path) => unsafe { env::set_var("PATH", path) },
            None => unsafe { env::remove_var("PATH") },
        }
        match previous_state {
            Some(path) => unsafe { env::set_var("MBUILD_TEST_BWRAP_STATE_ROOT", path) },
            None => unsafe { env::remove_var("MBUILD_TEST_BWRAP_STATE_ROOT") },
        }
        result
    }

    fn resolved_directory(root: &Path, name: &str) -> BuilderInputObject {
        let object_path = root.join(name);
        fs::create_dir_all(&object_path).unwrap();
        fs::write(object_path.join("README.txt"), b"hello\n").unwrap();
        BuilderInputObject {
            object_path,
            meta: Map::new(),
        }
    }

    fn resolved_file(root: &Path, name: &str) -> BuilderInputObject {
        let object_path = root.join(name);
        fs::write(&object_path, b"payload\n").unwrap();
        BuilderInputObject {
            object_path,
            meta: Map::new(),
        }
    }

    fn sample_inputs(root: &Path) -> BuilderInputs {
        BuilderInputs::new(BTreeMap::from([
            ("rootfs".to_string(), resolved_directory(root, "rootfs")),
            ("script".to_string(), resolved_file(root, "script.sh")),
            ("source".to_string(), resolved_directory(root, "src")),
            ("patch".to_string(), resolved_file(root, "patch.diff")),
        ]))
    }

    fn default_config() -> BinaryConfig {
        BinaryConfig {
            script_config: Some(serde_json::json!({
                "env": { "CC": "cc" }
            })),
            steps: vec![
                BuildStep {
                    name: "configure".to_string(),
                    run_as: StepUser::BuildUser,
                    cwd: "@{build}".to_string(),
                    argv: vec!["@{script}".to_string(), "@{source}".to_string()],
                    env: Map::from_iter([
                        (
                            "USER".to_string(),
                            Value::String("override-user".to_string()),
                        ),
                        ("CUSTOM".to_string(), Value::String("@{patch}".to_string())),
                    ]),
                },
                BuildStep {
                    name: "install".to_string(),
                    run_as: StepUser::Root,
                    cwd: "@{build}".to_string(),
                    argv: vec!["@{script}".to_string(), "install".to_string()],
                    env: Map::new(),
                },
            ],
            install: None,
        }
    }

    fn call_argv(state_root: &Path, index: usize) -> Vec<String> {
        fs::read_to_string(state_root.join(format!("call-{index}")).join("argv"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn option_positions(argv: &[String], option: &str) -> Vec<usize> {
        argv.iter()
            .enumerate()
            .filter_map(|(index, arg)| (arg == option).then_some(index))
            .collect()
    }

    #[test]
    fn container_builder_runs_fake_bwrap_once_per_step() {
        with_fake_bwrap(|state_root| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = ContainerBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap();

            assert_eq!(
                fs::read_to_string(result.staged_path.join("steps.txt")).unwrap(),
                "configure\ninstall\n"
            );
            assert!(result.meta.get("install").is_some());
            assert!(state_root.join("call-0").is_dir());
            assert!(state_root.join("call-1").is_dir());
            assert!(!state_root.join("call-2").exists());
        });
    }

    #[test]
    fn container_builder_uses_rootfs_overlay_and_input_overlays() {
        with_fake_bwrap(|state_root| {
            let temp = tempdir().unwrap();
            let inputs = sample_inputs(temp.path());
            let rootfs_path = inputs.get("rootfs").unwrap().object_path.clone();
            let source_path = inputs.get("source").unwrap().object_path.clone();
            let patch_path = inputs.get("patch").unwrap().object_path.clone();
            let mut cx = build_context(temp.path());

            ContainerBuilder
                .build_typed(default_config(), inputs, &mut cx)
                .unwrap();

            let first = call_argv(state_root, 0);
            assert_contains_sequence(&first, &["--overlay-src", rootfs_path.to_str().unwrap()]);
            assert_overlay_target(
                &first,
                "/",
                &cx.temp_dir.join(ROOTFS_OVERLAY_DIR_NAME).join("upper"),
                &cx.temp_dir.join(ROOTFS_OVERLAY_DIR_NAME).join("work"),
            );
            assert_not_contains_sequence(&first, &["--tmpfs", "/"]);
            assert_not_contains_sequence(
                &first,
                &[
                    "--ro-bind",
                    rootfs_path.join("README.txt").to_str().unwrap(),
                    "/README.txt",
                ],
            );
            assert_not_contains_sequence(
                &first,
                &["--ro-bind", rootfs_path.to_str().unwrap(), "/"],
            );
            assert_contains_sequence(
                &first,
                &[
                    "--bind",
                    cx.temp_dir.join(BUILD_DIR_NAME).to_str().unwrap(),
                    BUILD_DIR_MOUNT_PATH,
                ],
            );
            assert_contains_sequence(
                &first,
                &[
                    "--bind",
                    cx.temp_dir.join(OUTPUT_DIR_NAME).to_str().unwrap(),
                    OUT_DIR_MOUNT_PATH,
                ],
            );
            assert_contains_sequence(
                &first,
                &[
                    "--ro-bind",
                    patch_path.to_str().unwrap(),
                    "/__mbuild/inputs/patch",
                ],
            );
            assert_contains_sequence(&first, &["--overlay-src", source_path.to_str().unwrap()]);
            assert_overlay_target(
                &first,
                "/__mbuild/inputs/source",
                &cx.temp_dir
                    .join(OVERLAY_DIR_NAME)
                    .join("source")
                    .join("upper"),
                &cx.temp_dir
                    .join(OVERLAY_DIR_NAME)
                    .join("source")
                    .join("work"),
            );

            let overlay_positions = option_positions(&first, "--overlay");
            assert_eq!(overlay_positions.len(), 2);
            let root_overlay = overlay_for_target(&first, "/").unwrap();
            let root_upper = first[root_overlay + 1].clone();
            let root_work = first[root_overlay + 2].clone();
            let input_overlay = overlay_for_target(&first, "/__mbuild/inputs/source").unwrap();
            let input_upper = first[input_overlay + 1].clone();
            let input_work = first[input_overlay + 2].clone();

            let second = call_argv(state_root, 1);
            let second_root_overlay = overlay_for_target(&second, "/").unwrap();
            assert_eq!(second[second_root_overlay + 1], root_upper);
            assert_eq!(second[second_root_overlay + 2], root_work);
            let second_input_overlay =
                overlay_for_target(&second, "/__mbuild/inputs/source").unwrap();
            assert_eq!(second[second_input_overlay + 1], input_upper);
            assert_eq!(second[second_input_overlay + 2], input_work);
        });
    }

    #[test]
    fn container_builder_sets_env_and_user_context() {
        with_fake_bwrap(|state_root| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());

            ContainerBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap();

            let first = call_argv(state_root, 0);
            assert_contains_sequence(&first, &["--clearenv"]);
            assert_contains_sequence(&first, &["--setenv", "PATH", BASELINE_PATH]);
            assert_contains_sequence(&first, &["--setenv", "HOME", BASELINE_HOME]);
            assert_contains_sequence(&first, &["--setenv", "USER", BASELINE_USER]);
            assert_contains_sequence(&first, &["--setenv", "USER", "override-user"]);
            assert_contains_sequence(&first, &["--setenv", "CUSTOM", "/__mbuild/inputs/patch"]);
            assert_contains_sequence(&first, &["--setenv", STEP_NAME_ENV_VAR, "configure"]);
            assert_contains_sequence(&first, &["--uid"]);
            assert_contains_sequence(&first, &["--gid"]);

            let second = call_argv(state_root, 1);
            assert_contains_sequence(&second, &["--uid", "0", "--gid", "0"]);
        });
    }

    #[test]
    fn container_builder_rejects_missing_or_non_directory_rootfs() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut missing = sample_inputs(temp.path());
        let slots = BTreeMap::from([
            ("script".to_string(), missing.get("script").unwrap().clone()),
            ("source".to_string(), missing.get("source").unwrap().clone()),
        ]);
        missing = BuilderInputs::new(slots);
        let error = ContainerBuilder
            .build_typed(default_config(), missing, &mut cx)
            .unwrap_err();
        assert!(error.to_string().contains("required input slot 'rootfs'"));

        let mut cx = build_context(temp.path());
        let inputs = BuilderInputs::new(BTreeMap::from([
            (
                "rootfs".to_string(),
                resolved_file(temp.path(), "not-rootfs"),
            ),
            (
                "script".to_string(),
                resolved_file(temp.path(), "script-2.sh"),
            ),
        ]));
        let error = ContainerBuilder
            .build_typed(default_config(), inputs, &mut cx)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("rootfs input must resolve to a directory")
        );
    }

    fn assert_contains_sequence(argv: &[String], expected: &[&str]) {
        let found = argv.windows(expected.len()).any(|window| {
            window
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied())
        });
        assert!(found, "expected {:?} in argv:\n{:#?}", expected, argv);
    }

    fn assert_not_contains_sequence(argv: &[String], unexpected: &[&str]) {
        let found = argv.windows(unexpected.len()).any(|window| {
            window
                .iter()
                .map(String::as_str)
                .eq(unexpected.iter().copied())
        });
        assert!(
            !found,
            "did not expect {:?} in argv:\n{:#?}",
            unexpected, argv
        );
    }

    fn overlay_for_target(argv: &[String], target: &str) -> Option<usize> {
        option_positions(argv, "--overlay")
            .into_iter()
            .find(|index| argv.get(index + 3).is_some_and(|value| value == target))
    }

    fn assert_overlay_target(argv: &[String], target: &str, upper: &Path, work: &Path) {
        let index = overlay_for_target(argv, target)
            .unwrap_or_else(|| panic!("expected overlay target {target:?} in argv:\n{argv:#?}"));
        assert_eq!(argv[index + 1], upper.to_str().unwrap());
        assert_eq!(argv[index + 2], work.to_str().unwrap());
    }
}
