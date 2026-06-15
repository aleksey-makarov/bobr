use super::legacy_object::{
    FsTreeObjectMaterializer, IndexedTreeMergeInput, RuntimeFsTreeObjectMaterializer,
    current_epoch_nanos, elapsed_ms, load_fs_tree_compose_input, map_fs_tree_error,
    materialize_composed_tree_output, tree_subset_compose_details,
};
use crate::{
    BuildContext, BuilderInputObject, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder,
};
use globset::{Glob, GlobMatcher};
use mbuild_core::{
    BuildLogLevel, BuilderError, ComposedFsTree, ComposedFsTreeEntry, FsTreeComposeInput,
    FsTreeEntry, FsTreeManifest,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::time::Instant;

pub struct TreeSubsetBuilder;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeSubsetConfig {
    pub(super) include: Vec<String>,
}

static TREE_SUBSET_SPEC: InputSpec = InputSpec {
    required_inputs: &["tree"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for TreeSubsetBuilder {
    type Config = TreeSubsetConfig;

    fn tag(&self) -> &'static str {
        "TreeSubset"
    }

    fn spec(&self) -> &'static InputSpec {
        &TREE_SUBSET_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree_subset(config, inputs, cx, &RuntimeFsTreeObjectMaterializer)
    }
}

#[derive(Debug)]
pub(super) struct CompiledTreeSubsetPattern {
    pattern: String,
    matcher: GlobMatcher,
}

pub(super) fn build_tree_subset(
    config: TreeSubsetConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    materializer: &impl FsTreeObjectMaterializer,
) -> Result<StagedBuildResult, BuilderError> {
    let input = tree_subset_input(inputs.required("tree")?)?;
    let patterns = compile_tree_subset_patterns(&config.include)?;

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!(
            "selecting subset from fs-tree input with {} include pattern(s)",
            patterns.len()
        ),
    );

    let compose_start = Instant::now();
    let composed = compose_tree_subset(&input.compose, &patterns)?;
    let entry_count = composed.manifest().entries().len();
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "compose-done",
        format!("selected {entry_count} fs-tree entries"),
        None,
        None,
        tree_subset_compose_details(patterns.len(), entry_count, elapsed_ms(compose_start)),
    );

    let now_nanos = current_epoch_nanos()?;
    let output_path = cx.temp_dir.join(format!("tree-subset-{now_nanos}.obj"));

    cx.log_event(
        BuildLogLevel::Info,
        "materialize",
        format!("materializing fs-tree subset '{}'", output_path.display()),
    );

    let temp_dir = cx.temp_dir.clone();
    let compose_inputs = [input.compose];
    let object_hash = materialize_composed_tree_output(
        "fs-tree subset",
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

pub(super) fn compile_tree_subset_patterns(
    patterns: &[String],
) -> Result<Vec<CompiledTreeSubsetPattern>, BuilderError> {
    if patterns.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: include must contain at least one pattern".to_string(),
        ));
    }

    patterns
        .iter()
        .map(|pattern| {
            validate_tree_subset_pattern(pattern)?;
            let glob = Glob::new(pattern).map_err(|error| {
                BuilderError::InvalidRecipe(format!(
                    "invalid builder config: invalid include pattern '{}': {error}",
                    pattern
                ))
            })?;
            Ok(CompiledTreeSubsetPattern {
                pattern: pattern.clone(),
                matcher: glob.compile_matcher(),
            })
        })
        .collect()
}

pub(super) fn validate_tree_subset_pattern(pattern: &str) -> Result<(), BuilderError> {
    if pattern.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: include pattern must not be empty".to_string(),
        ));
    }
    let path = Path::new(pattern);
    if path.is_absolute() {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: include pattern '{pattern}' must be relative"
        )));
    }
    if pattern.contains('\\') {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: include pattern '{pattern}' must use '/' separators"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: include pattern '{pattern}' must not contain '..'"
        )));
    }
    Ok(())
}

pub(super) fn compose_tree_subset(
    input: &FsTreeComposeInput,
    patterns: &[CompiledTreeSubsetPattern],
) -> Result<ComposedFsTree, BuilderError> {
    let mut by_path = BTreeMap::new();
    for entry in input.manifest.entries() {
        by_path.insert(entry.path(), entry);
    }

    let mut selected = BTreeSet::<String>::new();
    for entry in input.manifest.entries() {
        let path = entry.path();
        if path.is_empty() {
            continue;
        }
        if tree_subset_path_matches(path, patterns) {
            selected.insert(path.to_string());
            add_tree_subset_parent_dirs(path, &by_path, &mut selected)?;
        }
    }

    if !selected.iter().any(|path| !path.is_empty()) {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: include patterns selected no paths".to_string(),
        ));
    }

    let mut manifest_entries = Vec::new();
    for entry in input.manifest.entries() {
        if selected.contains(entry.path()) {
            manifest_entries.push(entry.clone());
        }
    }
    let manifest = FsTreeManifest::from_entries(manifest_entries).map_err(map_fs_tree_error)?;
    let entries = manifest
        .entries()
        .iter()
        .map(|entry| match entry {
            FsTreeEntry::Directory { .. } => ComposedFsTreeEntry::Directory,
            FsTreeEntry::File { path, .. } => ComposedFsTreeEntry::File {
                source_path: input.root_dir.join(path),
            },
            FsTreeEntry::Symlink { path, .. } => ComposedFsTreeEntry::Symlink {
                source_path: input.root_dir.join(path),
            },
        })
        .collect();

    Ok(ComposedFsTree { manifest, entries })
}

pub(super) fn tree_subset_path_matches(path: &str, patterns: &[CompiledTreeSubsetPattern]) -> bool {
    patterns.iter().any(|pattern| {
        pattern.matcher.is_match(path)
            || pattern
                .pattern
                .strip_suffix("/**")
                .is_some_and(|prefix| path == prefix)
    })
}

pub(super) fn add_tree_subset_parent_dirs(
    path: &str,
    by_path: &BTreeMap<&str, &FsTreeEntry>,
    selected: &mut BTreeSet<String>,
) -> Result<(), BuilderError> {
    selected.insert(String::new());
    let mut remainder = path;
    while let Some((parent, _)) = remainder.rsplit_once('/') {
        let entry = by_path.get(parent).ok_or_else(|| {
            BuilderError::ExecutionFailed(format!(
                "input fs-tree manifest is missing parent directory '{parent}' for '{path}'"
            ))
        })?;
        if !matches!(entry, FsTreeEntry::Directory { .. }) {
            return Err(BuilderError::ExecutionFailed(format!(
                "input fs-tree manifest parent '{parent}' for '{path}' is not a directory"
            )));
        }
        selected.insert(parent.to_string());
        remainder = parent;
    }
    Ok(())
}

pub(super) fn tree_subset_input(
    object: &BuilderInputObject,
) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "TreeSubset input 'tree' is not a valid fs-tree object: {error}"
        ))
    })?;
    Ok(IndexedTreeMergeInput { compose })
}
