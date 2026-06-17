use super::legacy_object::{
    FsTreeObjectMaterializer, IndexedTreeMergeInput, RuntimeFsTreeObjectMaterializer,
    compose_rootfs_inputs_allowing_identical_leaf_overlap, current_epoch_nanos, elapsed_ms,
    load_fs_tree_compose_input, materialize_composed_tree_output, tree_merge_compose_details,
};
use crate::{
    BuildContext, BuilderInputPath, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder,
};
use mbuild_core::{BuildLogLevel, BuilderError};
use serde::Deserialize;
use std::time::Instant;

pub struct TreeMergeBuilder;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeMergeConfig {}

static TREE_MERGE_SPEC: InputSpec = InputSpec {
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for TreeMergeBuilder {
    type Config = TreeMergeConfig;

    fn tag(&self) -> &'static str {
        "TreeMerge"
    }

    fn spec(&self) -> &'static InputSpec {
        &TREE_MERGE_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree_merge(config, inputs, cx, &RuntimeFsTreeObjectMaterializer)
    }
}

pub(super) fn build_tree_merge(
    _config: TreeMergeConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    materializer: &impl FsTreeObjectMaterializer,
) -> Result<StagedBuildResult, BuilderError> {
    let inputs = inputs.extras(&TREE_MERGE_SPEC).collect::<Vec<_>>();
    if inputs.len() < 2 {
        return Err(BuilderError::ExecutionFailed(
            "TreeMerge builder requires at least two fs-tree inputs".to_string(),
        ));
    }

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!("merging {} fs-tree input(s)", inputs.len()),
    );

    let compose_start = Instant::now();
    let merge_inputs = inputs
        .iter()
        .map(|(name, object)| tree_merge_input(name, object))
        .collect::<Result<Vec<_>, _>>()?;
    let composed =
        compose_rootfs_inputs_allowing_identical_leaf_overlap("TreeMerge", &merge_inputs)?;
    let compose_inputs = merge_inputs
        .iter()
        .map(|input| input.compose.clone())
        .collect::<Vec<_>>();
    let entry_count = composed.manifest().entries().len();
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "compose-done",
        format!(
            "composed {} fs-tree input(s) into {} entries",
            inputs.len(),
            entry_count
        ),
        None,
        None,
        tree_merge_compose_details(inputs.len(), entry_count, elapsed_ms(compose_start)),
    );

    let now_nanos = current_epoch_nanos()?;
    let output_path = cx.temp_dir.join(format!("tree-merge-{now_nanos}.obj"));

    cx.log_event(
        BuildLogLevel::Info,
        "materialize",
        format!("materializing merged fs-tree '{}'", output_path.display()),
    );

    let temp_dir = cx.temp_dir.clone();
    let object_hash = materialize_composed_tree_output(
        "merged fs-tree",
        &output_path,
        &compose_inputs,
        &composed,
        &temp_dir,
        materializer,
        cx,
    )?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: Some(object_hash),
    })
}

pub(super) fn tree_merge_input(
    name: &str,
    object: &BuilderInputPath,
) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "TreeMerge input '{name}' is not a valid fs-tree object: {error}"
        ))
    })?;
    Ok(IndexedTreeMergeInput { compose })
}
