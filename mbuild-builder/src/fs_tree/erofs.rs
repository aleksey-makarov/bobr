use super::archive::archive_sources;
use super::legacy_object::{
    IndexedTreeMergeInput, compose_rootfs_inputs_allowing_identical_leaf_overlap,
    current_epoch_nanos, load_fs_tree_compose_input,
};
use crate::{
    BuildContext, BuilderInputObject, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder,
};
use mbuild_core::{BuildLogLevel, BuilderError, ComposedFsTree, FsTreeComposeInput};
use mbuild_runtime::FsTreeArchiveInput;
use serde::Deserialize;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct ErofsRootfsBuilder;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErofsRootfsConfig {
    #[serde(default)]
    pub(super) compression: Option<String>,
    #[serde(default)]
    pub(super) label: Option<String>,
}

static EROFS_ROOTFS_SPEC: InputSpec = InputSpec {
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for ErofsRootfsBuilder {
    type Config = ErofsRootfsConfig;

    fn tag(&self) -> &'static str {
        "ErofsRootfs"
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
        build_erofs_rootfs(
            config,
            inputs,
            cx,
            &RuntimeErofsTarWriter,
            &PathProgramResolver,
        )
    }
}

pub(super) fn build_erofs_rootfs(
    config: ErofsRootfsConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    tar_writer: &impl ErofsTarWriter,
    program_resolver: &impl ProgramResolver,
) -> Result<StagedBuildResult, BuilderError> {
    validate_erofs_config(&config)?;
    let inputs = inputs.extras(&EROFS_ROOTFS_SPEC).collect::<Vec<_>>();
    if inputs.is_empty() {
        return Err(BuilderError::ExecutionFailed(
            "ErofsRootfs builder requires at least one fs-tree input".to_string(),
        ));
    }

    let mkfs_erofs = program_resolver.resolve("mkfs.erofs")?;

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!("composing {} fs-tree input(s) for EROFS", inputs.len()),
    );

    let merge_inputs = inputs
        .iter()
        .map(|(name, object)| erofs_rootfs_input(name, object))
        .collect::<Result<Vec<_>, _>>()?;
    let compose_inputs = merge_inputs
        .iter()
        .map(|input| input.compose.clone())
        .collect::<Vec<_>>();
    let composed =
        compose_rootfs_inputs_allowing_identical_leaf_overlap("ErofsRootfs", &merge_inputs)?;

    let now_nanos = current_epoch_nanos()?;
    let tar_path = cx.temp_dir.join(format!("erofs-rootfs-{now_nanos}.tar"));
    let output_path = cx.temp_dir.join(format!("erofs-rootfs-{now_nanos}.erofs"));

    cx.log_event(
        BuildLogLevel::Info,
        "tar",
        format!(
            "writing deterministic EROFS source tar '{}'",
            tar_path.display()
        ),
    );
    tar_writer.write_tar(&compose_inputs, &composed, &tar_path, &cx.temp_dir)?;

    cx.log_event(
        BuildLogLevel::Info,
        "mkfs",
        format!("creating EROFS image '{}'", output_path.display()),
    );
    run_mkfs_erofs(&mkfs_erofs, &config, &output_path, &tar_path)?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: None,
    })
}

pub(super) fn validate_erofs_config(config: &ErofsRootfsConfig) -> Result<(), BuilderError> {
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

pub(super) fn erofs_rootfs_input(
    name: &str,
    object: &BuilderInputObject,
) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "ErofsRootfs input '{name}' is not a valid fs-tree object: {error}"
        ))
    })?;
    Ok(IndexedTreeMergeInput { compose })
}

pub(super) trait ErofsTarWriter {
    fn write_tar(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_tar: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError>;
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RuntimeErofsTarWriter;

impl ErofsTarWriter for RuntimeErofsTarWriter {
    fn write_tar(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_tar: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError> {
        let archive_inputs = inputs
            .iter()
            .map(|input| FsTreeArchiveInput {
                root_dir: input.root_dir.clone(),
            })
            .collect::<Vec<_>>();
        let sources = archive_sources("ErofsRootfs", inputs, composed)?;
        mbuild_runtime::write_fs_tree_tar_in_ownership_namespace(
            &archive_inputs,
            composed.manifest(),
            &sources,
            output_tar,
            workspace,
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
    }
}

pub(super) trait ProgramResolver {
    fn resolve(&self, program: &str) -> Result<PathBuf, BuilderError>;
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PathProgramResolver;

impl ProgramResolver for PathProgramResolver {
    fn resolve(&self, program: &str) -> Result<PathBuf, BuilderError> {
        find_program_in_path(program).ok_or_else(|| {
            BuilderError::ExecutionFailed(format!(
                "required tool '{program}' was not found in PATH; install erofs-utils"
            ))
        })
    }
}

pub(super) fn find_program_in_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable_file(&path).then_some(path);
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub(super) fn is_executable_file(path: &Path) -> bool {
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

pub(super) fn run_mkfs_erofs(
    mkfs_erofs: &Path,
    config: &ErofsRootfsConfig,
    output_path: &Path,
    tar_path: &Path,
) -> Result<(), BuilderError> {
    let mut command = Command::new(mkfs_erofs);
    command
        .arg("--tar=f")
        .arg("--sort=path")
        .arg("-T")
        .arg("0")
        .arg("-U")
        .arg("clear");
    if let Some(label) = config.label.as_ref() {
        command.arg("-L").arg(label);
    }
    if let Some(compression) = config.compression.as_ref() {
        command.arg("-z").arg(compression);
    }
    command.arg(output_path).arg(tar_path);

    let output = command.output().map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to execute '{}': {error}",
            mkfs_erofs.display()
        ))
    })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(BuilderError::ExecutionFailed(format!(
        "mkfs.erofs failed with status {}: {}",
        output.status,
        stderr.trim_end()
    )))
}
