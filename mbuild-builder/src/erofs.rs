use crate::{BuildContext, BuilderInputs, InputSlot, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use mbuild_core::{BuildLogLevel, BuilderError};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const OUTPUT_FILE_NAME: &str = "erofs-rootfs.erofs";

pub struct ErofsRootfsNewBuilder;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErofsRootfsNewConfig {
    #[serde(default)]
    pub compression: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

static EROFS_ROOTFS_NEW_SPEC: InputSpec = InputSpec {
    required_inputs: &[InputSlot::fs_tree_root("tree")],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for ErofsRootfsNewBuilder {
    type Config = ErofsRootfsNewConfig;

    fn tag(&self) -> &'static str {
        "ErofsRootfsNew"
    }

    fn spec(&self) -> &'static InputSpec {
        &EROFS_ROOTFS_NEW_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_erofs_config(&config)?;
        let source_root = inputs.required("tree")?.path.clone();
        let mkfs_erofs = find_program_in_path("mkfs.erofs").ok_or_else(|| {
            BuilderError::ExecutionFailed(
                "required tool 'mkfs.erofs' was not found in PATH; install erofs-utils".to_string(),
            )
        })?;
        let output_path = cx.temp_dir.join(OUTPUT_FILE_NAME);

        cx.log_event(
            BuildLogLevel::Info,
            "mkfs",
            format!(
                "creating EROFS image '{}' from materialized fs-tree root '{}'",
                output_path.display(),
                source_root.display()
            ),
        );

        cx.runtime()
            .run(
                &ErofsRootfsFunction,
                ErofsRootfsInput {
                    source_root,
                    mkfs_erofs,
                    output_path: output_path.clone(),
                    compression: config.compression,
                    label: config.label,
                },
            )
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;

        Ok(StagedBuildResult {
            staged_path: output_path,
            object_hash: None,
        })
    }
}

fn validate_erofs_config(config: &ErofsRootfsNewConfig) -> Result<(), BuilderError> {
    if matches!(config.compression.as_deref(), Some("")) {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: compression must be null or a non-empty string".to_string(),
        ));
    }
    if matches!(config.label.as_deref(), Some("")) {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: label must be null or a non-empty string".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ErofsRootfsFunction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ErofsRootfsInput {
    source_root: PathBuf,
    mkfs_erofs: PathBuf,
    output_path: PathBuf,
    compression: Option<String>,
    label: Option<String>,
}

impl RuntimeFunction for ErofsRootfsFunction {
    type Input = ErofsRootfsInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "erofs-rootfs"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        build_erofs_rootfs_image(input).map_err(|error| RuntimeError::new(error.to_string()))
    }
}

fn build_erofs_rootfs_image(input: ErofsRootfsInput) -> Result<(), ErofsRootfsError> {
    if input.output_path.exists() {
        return Err(ErofsRootfsError::InvalidInput(format!(
            "ErofsRootfsNew output path already exists: '{}'",
            input.output_path.display()
        )));
    }

    run_mkfs_erofs(
        &input.mkfs_erofs,
        &input.output_path,
        &input.source_root,
        input.compression.as_deref(),
        input.label.as_deref(),
    )
}

fn run_mkfs_erofs(
    mkfs_erofs: &Path,
    output_path: &Path,
    source_root: &Path,
    compression: Option<&str>,
    label: Option<&str>,
) -> Result<(), ErofsRootfsError> {
    let mut command = Command::new(mkfs_erofs);
    command
        .arg("--sort=path")
        .arg("-T")
        .arg("0")
        .arg("-U")
        .arg("clear");
    if let Some(label) = label {
        command.arg("-L").arg(label);
    }
    if let Some(compression) = compression {
        command.arg("-z").arg(compression);
    }
    command.arg(output_path).arg(source_root);

    let output = command.output().map_err(|error| {
        ErofsRootfsError::Io(format!(
            "failed to execute '{}': {error}",
            mkfs_erofs.display()
        ))
    })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(ErofsRootfsError::Command(format!(
        "mkfs.erofs failed with status {}: {}",
        output.status,
        stderr.trim_end()
    )))
}

fn find_program_in_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return executable_absolute_path(&path);
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if let Some(path) = executable_absolute_path(&candidate) {
            return Some(path);
        }
    }
    None
}

fn executable_absolute_path(path: &Path) -> Option<PathBuf> {
    if !is_executable_file(path) {
        return None;
    }
    let absolute = fs::canonicalize(path).ok()?;
    absolute.is_absolute().then_some(absolute)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Debug)]
enum ErofsRootfsError {
    InvalidInput(String),
    Io(String),
    Command(String),
}

impl std::fmt::Display for ErofsRootfsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(message) | Self::Io(message) | Self::Command(message) => {
                formatter.write_str(message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BuilderInputPath;
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn input_spec_is_single_tree_input() {
        assert_eq!(TypedBuilder::tag(&ErofsRootfsNewBuilder), "ErofsRootfsNew");
        assert_eq!(
            EROFS_ROOTFS_NEW_SPEC.required_inputs,
            &[InputSlot::fs_tree_root("tree")]
        );
        assert!(!EROFS_ROOTFS_NEW_SPEC.allow_extra_inputs);
    }

    #[test]
    fn build_rejects_missing_tree_input() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = ErofsRootfsNewBuilder
            .build_typed(
                ErofsRootfsNewConfig {
                    compression: None,
                    label: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("required input slot 'tree'"));
    }

    #[test]
    fn build_rejects_empty_compression_and_label() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "tree",
            BuilderInputPath {
                path: temp.path().join("root"),
            },
        );

        let error = ErofsRootfsNewBuilder
            .build_typed(
                ErofsRootfsNewConfig {
                    compression: Some(String::new()),
                    label: None,
                },
                inputs.clone(),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("compression"));

        let error = ErofsRootfsNewBuilder
            .build_typed(
                ErofsRootfsNewConfig {
                    compression: None,
                    label: Some(String::new()),
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("label"));
    }

    #[test]
    fn runtime_function_runs_mkfs_on_materialized_root() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("tool"), b"tool\n").unwrap();
        fs::set_permissions(source.join("tool"), fs::Permissions::from_mode(0o644)).unwrap();
        let log_path = temp.path().join("mkfs-args.txt");
        let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, false);
        let output_path = temp.path().join("rootfs.erofs");

        build_erofs_rootfs_image(ErofsRootfsInput {
            source_root: source.clone(),
            mkfs_erofs: mkfs,
            output_path: output_path.clone(),
            compression: Some("lz4".to_string()),
            label: Some("root".to_string()),
        })
        .unwrap();

        assert_eq!(
            fs::read_to_string(&output_path).unwrap(),
            "fake erofs image\n"
        );
        let args = fs::read_to_string(log_path).unwrap();
        assert!(args.contains("--sort=path\n"));
        assert!(args.contains("-T\n0\n"));
        assert!(args.contains("-U\nclear\n"));
        assert!(args.contains("-L\nroot\n"));
        assert!(args.contains("-z\nlz4\n"));
        assert!(args.contains(&format!("{}\n", source.display())));
    }

    #[test]
    fn runtime_function_reports_mkfs_stderr_on_failure() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("tool"), b"tool\n").unwrap();
        let mkfs = install_fake_mkfs_erofs(temp.path(), &temp.path().join("mkfs-args.txt"), true);

        let error = build_erofs_rootfs_image(ErofsRootfsInput {
            source_root: source,
            mkfs_erofs: mkfs,
            output_path: temp.path().join("rootfs.erofs"),
            compression: None,
            label: None,
        })
        .unwrap_err();

        assert!(error.to_string().contains("mkfs.erofs failed"));
        assert!(error.to_string().contains("fake mkfs failure"));
    }

    fn install_fake_mkfs_erofs(root: &Path, log_path: &Path, fail: bool) -> PathBuf {
        let script_path = root.join(if fail {
            "mkfs.erofs.fail"
        } else {
            "mkfs.erofs"
        });
        let failure = if fail {
            "printf '%s\\n' 'fake mkfs failure' >&2\nexit 42\n"
        } else {
            ""
        };
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > {}\n{failure}last=''\nprev=''\nfor arg in \"$@\"; do\n  prev=\"$last\"\n  last=\"$arg\"\ndone\nprintf 'fake erofs image\\n' > \"$prev\"\n",
                shell_quote(log_path)
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
        script_path
    }

    fn shell_quote(path: &Path) -> String {
        let raw = path.to_str().unwrap();
        format!("'{}'", raw.replace('\'', "'\\''"))
    }

    #[test]
    fn fake_mkfs_script_quotes_paths() -> io::Result<()> {
        let temp = tempdir().unwrap();
        let log_path = temp.path().join("path with spaces");
        let script = install_fake_mkfs_erofs(temp.path(), &log_path, false);
        assert!(script.is_file());
        Ok(())
    }
}
