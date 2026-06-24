use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSlot, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_store::{
    StoreError,
    fs_tree::{FsTreeManifest, subset_manifest},
};
use mbuild_core::BuildLogLevel;
use serde::Deserialize;

pub struct TreeSubsetBuilder;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeSubsetConfig {
    include: Vec<String>,
}

static TREE_SUBSET_SPEC: InputSpec = InputSpec {
    required_inputs: &[InputSlot::object("tree")],
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
        build_tree_subset(config, inputs, cx)
    }
}

fn build_tree_subset(
    config: TreeSubsetConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<StagedBuildResult, BuilderError> {
    let input = inputs.required("tree")?;
    let manifest = FsTreeManifest::read_canonical(&input.path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "TreeSubset input 'tree' is not a valid fs-tree manifest: {error}"
        ))
    })?;

    cx.log_event(
        BuildLogLevel::Info,
        "subset",
        format!(
            "selecting fs-tree manifest subset with {} include pattern(s)",
            config.include.len()
        ),
    );

    let subset = subset_manifest(&manifest, &config.include).map_err(map_subset_error)?;
    let output_path = cx.temp_dir.join("fs-tree-subset-manifest.jsonl");
    subset.write_canonical(&output_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to write fs-tree subset manifest '{}': {error}",
            output_path.display()
        ))
    })?;

    cx.log_event(
        BuildLogLevel::Info,
        "subset",
        format!(
            "wrote fs-tree subset manifest with {} entries",
            subset.entries().len()
        ),
    );

    Ok(StagedBuildResult {
        staged_path: output_path,
    })
}

fn map_subset_error(error: StoreError) -> BuilderError {
    match error {
        StoreError::InvalidInput(message) => BuilderError::InvalidRecipe(message),
        error => BuilderError::ExecutionFailed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Builder, BuilderInputPath};
    use bobr_store::fs_tree::{FsFileHash, FsTreeEntry};
    use std::collections::BTreeMap;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn hash() -> FsFileHash {
        FsFileHash::from_str("1111111111111111111111111111111111111111111111111111111111111111")
            .unwrap()
    }

    fn other_hash() -> FsFileHash {
        FsFileHash::from_str("2222222222222222222222222222222222222222222222222222222222222222")
            .unwrap()
    }

    fn manifest(entries: Vec<FsTreeEntry>) -> FsTreeManifest {
        FsTreeManifest::from_entries(entries).unwrap()
    }

    fn write_manifest(
        root: &std::path::Path,
        name: &str,
        manifest: &FsTreeManifest,
    ) -> BuilderInputPath {
        let path = root.join(name);
        manifest.write_canonical(&path).unwrap();
        BuilderInputPath { path }
    }

    fn inputs(entries: Vec<(&str, BuilderInputPath)>) -> BuilderInputs {
        BuilderInputs::new(
            entries
                .into_iter()
                .map(|(name, input)| (name.to_string(), input))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn root() -> FsTreeEntry {
        FsTreeEntry::directory("", 0, 0, 0o755)
    }

    #[test]
    fn spec_requires_tree_input_only() {
        assert_eq!(TypedBuilder::tag(&TreeSubsetBuilder), "TreeSubset");
        assert_eq!(TREE_SUBSET_SPEC.required_inputs.len(), 1);
        assert_eq!(
            TREE_SUBSET_SPEC.required_inputs[0],
            InputSlot::object("tree")
        );
        assert!(TREE_SUBSET_SPEC.optional_inputs.is_empty());
        assert!(!TREE_SUBSET_SPEC.allow_extra_inputs);
    }

    #[test]
    fn build_selects_subset_and_parent_directories() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::directory("usr/bin", 0, 0, 0o755),
            FsTreeEntry::file("usr/bin/tool", hash()),
            FsTreeEntry::directory("usr/lib", 0, 0, 0o755),
            FsTreeEntry::file("usr/lib/libtool.so", other_hash()),
            FsTreeEntry::directory("etc", 0, 0, 0o755),
            FsTreeEntry::symlink("etc/tool", 0, 0, "../usr/bin/tool"),
        ]);

        let result = TreeSubsetBuilder
            .build_typed(
                TreeSubsetConfig {
                    include: vec!["usr/bin/*".to_string(), "etc/tool".to_string()],
                },
                inputs(vec![(
                    "tree",
                    write_manifest(temp.path(), "tree.jsonl", &input),
                )]),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            result.staged_path,
            temp.path()
                .join("tmp")
                .join("fs-tree-subset-manifest.jsonl")
        );
        let subset = FsTreeManifest::read_canonical(&result.staged_path).unwrap();
        let paths = subset
            .entries()
            .iter()
            .map(FsTreeEntry::path)
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec!["", "etc", "etc/tool", "usr", "usr/bin", "usr/bin/tool"]
        );
    }

    #[test]
    fn build_double_star_pattern_selects_prefix_path() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::directory("usr/bin", 0, 0, 0o755),
            FsTreeEntry::file("usr/bin/tool", hash()),
        ]);

        let result = TreeSubsetBuilder
            .build_typed(
                TreeSubsetConfig {
                    include: vec!["usr/bin/**".to_string()],
                },
                inputs(vec![(
                    "tree",
                    write_manifest(temp.path(), "tree.jsonl", &input),
                )]),
                &mut cx,
            )
            .unwrap();

        let subset = FsTreeManifest::read_canonical(&result.staged_path).unwrap();
        let paths = subset
            .entries()
            .iter()
            .map(FsTreeEntry::path)
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["", "usr", "usr/bin", "usr/bin/tool"]);
    }

    #[test]
    fn build_rejects_empty_include() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let input = manifest(vec![root(), FsTreeEntry::file("a", hash())]);

        let error = TreeSubsetBuilder
            .build_typed(
                TreeSubsetConfig { include: vec![] },
                inputs(vec![(
                    "tree",
                    write_manifest(temp.path(), "tree.jsonl", &input),
                )]),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
        assert!(error.to_string().contains("must contain at least one"));
    }

    #[test]
    fn build_rejects_no_match_include() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let input = manifest(vec![root(), FsTreeEntry::file("a", hash())]);

        let error = TreeSubsetBuilder
            .build_typed(
                TreeSubsetConfig {
                    include: vec!["missing/**".to_string()],
                },
                inputs(vec![(
                    "tree",
                    write_manifest(temp.path(), "tree.jsonl", &input),
                )]),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
        assert!(error.to_string().contains("selected no paths"));
    }

    #[test]
    fn build_reports_tree_input_for_invalid_manifest() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let invalid_path = temp.path().join("invalid.jsonl");
        std::fs::write(&invalid_path, b"not a manifest\n").unwrap();

        let error = TreeSubsetBuilder
            .build_typed(
                TreeSubsetConfig {
                    include: vec!["**".to_string()],
                },
                inputs(vec![("tree", BuilderInputPath { path: invalid_path })]),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("input 'tree'"));
    }

    #[test]
    fn erased_config_rejects_unknown_fields() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = TreeSubsetBuilder
            .build_erased(
                serde_json::json!({"include": ["**"], "extra": true}),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }
}
