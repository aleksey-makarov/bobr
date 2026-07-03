use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_core::BuildLogLevel;
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const OUTPUT_FILE_NAME: &str = "erofs-rootfs.erofs";

/// Fixed build timestamp for reproducible images (1980-01-01 UTC). mkfs.erofs
/// treats `-T 0` as unset and falls back to the ambient `SOURCE_DATE_EPOCH`, so
/// the superblock build time leaked the host environment; pin both the `-T`
/// argument and `SOURCE_DATE_EPOCH` to this non-zero value. Matches the sandbox
/// `SOURCE_DATE_EPOCH` so the whole build agrees on one epoch.
const REPRODUCIBLE_SOURCE_DATE_EPOCH: &str = "315532800";

/// Builds an EROFS rootfs image from an fs-tree (the `tree` input).
#[derive(Debug)]
pub struct ErofsRootfsBuilder;

/// Configuration for [`ErofsRootfsBuilder`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErofsRootfsConfig {
    /// Optional compression algorithm (e.g. `"lz4hc"`); uses the mkfs default
    /// when `None`.
    #[serde(default)]
    pub compression: Option<String>,
    /// Optional filesystem label.
    #[serde(default)]
    pub label: Option<String>,
}

static EROFS_ROOTFS_SPEC: InputSpec = InputSpec {
    required_inputs: &["_tree"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for ErofsRootfsBuilder {
    type Config = ErofsRootfsConfig;

    fn tag(&self) -> &'static str {
        "ErofsRootfs"
    }

    fn impl_version(&self) -> &'static str {
        "1"
    }

    fn spec(&self) -> &'static InputSpec {
        &EROFS_ROOTFS_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_erofs_config(&config)?;
        let source_root = inputs.required("_tree")?.clone();
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
        })
    }
}

fn validate_erofs_config(config: &ErofsRootfsConfig) -> Result<(), BuilderError> {
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
            "ErofsRootfs output path already exists: '{}'",
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
    let mut command = mkfs_erofs_command(mkfs_erofs, output_path, source_root, compression, label);
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

fn mkfs_erofs_command(
    mkfs_erofs: &Path,
    output_path: &Path,
    source_root: &Path,
    compression: Option<&str>,
    label: Option<&str>,
) -> Command {
    let mut command = Command::new(mkfs_erofs);
    command
        // Pin the build time explicitly and override any ambient
        // SOURCE_DATE_EPOCH so the superblock time is host-independent.
        .env("SOURCE_DATE_EPOCH", REPRODUCIBLE_SOURCE_DATE_EPOCH)
        .arg("--sort=path")
        .arg("-T")
        .arg(REPRODUCIBLE_SOURCE_DATE_EPOCH)
        .arg("-U")
        .arg("clear");
    if let Some(label) = label {
        command.arg("-L").arg(label);
    }
    if let Some(compression) = compression {
        command.arg("-z").arg(compression);
    }
    command.arg(output_path).arg(source_root);
    command
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
    use bobr_store::fs_tree::FsTree;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn input_spec_is_single_tree_input() {
        assert_eq!(TypedBuilder::tag(&ErofsRootfsBuilder), "ErofsRootfs");
        assert_eq!(EROFS_ROOTFS_SPEC.required_inputs, &["_tree"]);
        assert!(!EROFS_ROOTFS_SPEC.allow_extra_inputs);
    }

    #[test]
    fn build_rejects_missing_tree_input() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(
            temp.path().join("tmp"),
            FsTree::new(temp.path().to_path_buf()),
        );

        let error = ErofsRootfsBuilder
            .build_typed(
                ErofsRootfsConfig {
                    compression: None,
                    label: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("required input slot '_tree'"));
    }

    #[test]
    fn build_rejects_empty_compression_and_label() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(
            temp.path().join("tmp"),
            FsTree::new(temp.path().to_path_buf()),
        );
        let mut inputs = BuilderInputs::empty();
        inputs.insert("_tree", temp.path().join("root"));

        let error = ErofsRootfsBuilder
            .build_typed(
                ErofsRootfsConfig {
                    compression: Some(String::new()),
                    label: None,
                },
                inputs.clone(),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("compression"));

        let error = ErofsRootfsBuilder
            .build_typed(
                ErofsRootfsConfig {
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
    fn runtime_function_runs_successful_mkfs_command() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("tool"), b"tool\n").unwrap();
        let mkfs = true_program_for_success_test();
        let output_path = temp.path().join("rootfs.erofs");

        build_erofs_rootfs_image(ErofsRootfsInput {
            source_root: source,
            mkfs_erofs: mkfs,
            output_path,
            compression: Some("lz4".to_string()),
            label: Some("root".to_string()),
        })
        .unwrap();
    }

    #[test]
    fn mkfs_command_uses_reproducible_arguments() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let output_path = temp.path().join("rootfs.erofs");
        let mkfs = PathBuf::from("/test/mkfs.erofs");
        let command = mkfs_erofs_command(&mkfs, &output_path, &source, Some("lz4"), Some("root"));

        assert_eq!(command.get_program(), mkfs.as_os_str());
        let args = command
            .get_args()
            .map(|arg| arg.to_os_string())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                OsString::from("--sort=path"),
                OsString::from("-T"),
                OsString::from("315532800"),
                OsString::from("-U"),
                OsString::from("clear"),
                OsString::from("-L"),
                OsString::from("root"),
                OsString::from("-z"),
                OsString::from("lz4"),
                output_path.as_os_str().to_os_string(),
                source.as_os_str().to_os_string(),
            ]
        );
        // SOURCE_DATE_EPOCH is pinned so the superblock build time does not
        // depend on the ambient environment.
        let source_date_epoch = command
            .get_envs()
            .find_map(|(key, value)| (key == OsStr::new("SOURCE_DATE_EPOCH")).then_some(value));
        assert_eq!(source_date_epoch, Some(Some(OsStr::new("315532800"))));
    }

    #[test]
    fn runtime_function_reports_mkfs_stderr_on_failure() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("tool"), b"tool\n").unwrap();
        let mkfs = env_program_for_failure_test();

        let error = build_erofs_rootfs_image(ErofsRootfsInput {
            source_root: source,
            mkfs_erofs: mkfs,
            output_path: temp.path().join("rootfs.erofs"),
            compression: None,
            label: None,
        })
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("mkfs.erofs failed"), "{message}");
        assert!(message.contains("--sort=path"), "{message}");
    }

    fn true_program_for_success_test() -> PathBuf {
        program_for_test(
            "true",
            &[
                "/usr/bin/true",
                "/bin/true",
                "/run/current-system/sw/bin/true",
            ],
        )
    }

    fn env_program_for_failure_test() -> PathBuf {
        program_for_test(
            "env",
            &["/usr/bin/env", "/bin/env", "/run/current-system/sw/bin/env"],
        )
    }

    fn program_for_test(program: &str, absolute_paths: &[&str]) -> PathBuf {
        for path in absolute_paths {
            let path = PathBuf::from(path);
            if fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                return path;
            }
        }
        find_program_in_path(program)
            .unwrap_or_else(|| panic!("test environment must provide {program}"))
    }
}
