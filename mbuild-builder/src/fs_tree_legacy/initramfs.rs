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
use std::path::Path;

pub struct InitramfsBuilder;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitramfsConfig {}

static INITRAMFS_SPEC: InputSpec = InputSpec {
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for InitramfsBuilder {
    type Config = InitramfsConfig;

    fn tag(&self) -> &'static str {
        "Initramfs"
    }

    fn spec(&self) -> &'static InputSpec {
        &INITRAMFS_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_initramfs(config, inputs, cx, &RuntimeInitramfsWriter)
    }
}

pub(super) fn build_initramfs(
    _config: InitramfsConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    initramfs_writer: &impl InitramfsWriter,
) -> Result<StagedBuildResult, BuilderError> {
    let inputs = inputs.extras(&INITRAMFS_SPEC).collect::<Vec<_>>();
    if inputs.is_empty() {
        return Err(BuilderError::ExecutionFailed(
            "Initramfs builder requires at least one fs-tree input".to_string(),
        ));
    }

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!("composing {} fs-tree input(s) for initramfs", inputs.len()),
    );

    let merge_inputs = inputs
        .iter()
        .map(|(name, object)| initramfs_input(name, object))
        .collect::<Result<Vec<_>, _>>()?;
    let compose_inputs = merge_inputs
        .iter()
        .map(|input| input.compose.clone())
        .collect::<Vec<_>>();
    let composed =
        compose_rootfs_inputs_allowing_identical_leaf_overlap("Initramfs", &merge_inputs)?;

    let now_nanos = current_epoch_nanos()?;
    let output_path = cx.temp_dir.join(format!("initramfs-{now_nanos}.img"));

    cx.log_event(
        BuildLogLevel::Info,
        "initramfs",
        format!(
            "writing deterministic initramfs '{}'",
            output_path.display()
        ),
    );
    initramfs_writer.write_initramfs(&compose_inputs, &composed, &output_path, &cx.temp_dir)?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: None,
    })
}

pub(super) fn initramfs_input(
    name: &str,
    object: &BuilderInputObject,
) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "Initramfs input '{name}' is not a valid fs-tree object: {error}"
        ))
    })?;
    Ok(IndexedTreeMergeInput { compose })
}

pub(super) trait InitramfsWriter {
    fn write_initramfs(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_initramfs: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError>;
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RuntimeInitramfsWriter;

impl InitramfsWriter for RuntimeInitramfsWriter {
    fn write_initramfs(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_initramfs: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError> {
        let archive_inputs = inputs
            .iter()
            .map(|input| FsTreeArchiveInput {
                root_dir: input.root_dir.clone(),
            })
            .collect::<Vec<_>>();
        let sources = archive_sources("rootfs", inputs, composed)?;
        mbuild_runtime::write_fs_tree_initramfs_in_ownership_namespace(
            &archive_inputs,
            composed.manifest(),
            &sources,
            output_initramfs,
            workspace,
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
    }
}
