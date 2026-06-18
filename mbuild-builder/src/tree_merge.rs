use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_store::fs_tree::{FsTreeManifest, merge_manifests};
use mbuild_core::{BuildLogLevel, BuilderError};
use serde::Deserialize;

pub struct TreeMergeNewBuilder;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeMergeNewConfig {}

static TREE_MERGE_NEW_SPEC: InputSpec = InputSpec {
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for TreeMergeNewBuilder {
    type Config = TreeMergeNewConfig;

    fn tag(&self) -> &'static str {
        "TreeMerge"
    }

    fn spec(&self) -> &'static InputSpec {
        &TREE_MERGE_NEW_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree_merge(config, inputs, cx)
    }
}

fn build_tree_merge(
    _config: TreeMergeNewConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<StagedBuildResult, BuilderError> {
    let inputs = inputs.extras(&TREE_MERGE_NEW_SPEC).collect::<Vec<_>>();
    if inputs.len() < 2 {
        return Err(BuilderError::ExecutionFailed(
            "TreeMerge builder requires at least two fs-tree manifest inputs".to_string(),
        ));
    }

    cx.log_event(
        BuildLogLevel::Info,
        "merge",
        format!("merging {} fs-tree manifest input(s)", inputs.len()),
    );

    let manifests = inputs
        .iter()
        .map(|(name, input)| {
            FsTreeManifest::read_canonical(&input.path).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "TreeMerge input '{name}' is not a valid fs-tree v2 manifest: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let merged = merge_manifests(&manifests)
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let output_path = cx.temp_dir.join("fs-tree-merge-manifest-v2.jsonl");
    merged.write_canonical(&output_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to write merged fs-tree manifest '{}': {error}",
            output_path.display()
        ))
    })?;

    cx.log_event(
        BuildLogLevel::Info,
        "merge",
        format!(
            "wrote merged fs-tree manifest with {} entries",
            merged.entries().len()
        ),
    );

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: None,
    })
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
    fn spec_accepts_extra_inputs_only() {
        assert_eq!(TypedBuilder::tag(&TreeMergeNewBuilder), "TreeMerge");
        assert!(TREE_MERGE_NEW_SPEC.required_inputs.is_empty());
        assert!(TREE_MERGE_NEW_SPEC.optional_inputs.is_empty());
        assert!(TREE_MERGE_NEW_SPEC.allow_extra_inputs);
    }

    #[test]
    fn build_rejects_fewer_than_two_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = TreeMergeNewBuilder
            .build_typed(TreeMergeNewConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("requires at least two"));
    }

    #[test]
    fn build_merges_disjoint_manifests_and_writes_canonical_output() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let left = manifest(vec![
            root(),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::file("usr/tool", hash()),
        ]);
        let right = manifest(vec![
            root(),
            FsTreeEntry::directory("etc", 1, 2, 0o750),
            FsTreeEntry::symlink("etc/tool", 1, 2, "../usr/tool"),
        ]);

        let result = TreeMergeNewBuilder
            .build_typed(
                TreeMergeNewConfig {},
                inputs(vec![
                    ("left", write_manifest(temp.path(), "left.jsonl", &left)),
                    ("right", write_manifest(temp.path(), "right.jsonl", &right)),
                ]),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            result.staged_path,
            temp.path()
                .join("tmp")
                .join("fs-tree-merge-manifest-v2.jsonl")
        );
        assert!(result.object_hash.is_none());
        let merged = FsTreeManifest::read_canonical(&result.staged_path).unwrap();
        let paths = merged
            .entries()
            .iter()
            .map(FsTreeEntry::path)
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["", "etc", "etc/tool", "usr", "usr/tool"]);
    }

    #[test]
    fn build_allows_identical_overlap() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let manifest = manifest(vec![
            root(),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file("bin/tool", hash()),
        ]);

        TreeMergeNewBuilder
            .build_typed(
                TreeMergeNewConfig {},
                inputs(vec![
                    (
                        "first",
                        write_manifest(temp.path(), "first.jsonl", &manifest),
                    ),
                    (
                        "second",
                        write_manifest(temp.path(), "second.jsonl", &manifest),
                    ),
                ]),
                &mut cx,
            )
            .unwrap();
    }

    #[test]
    fn build_reports_named_input_for_invalid_manifest() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let valid = manifest(vec![root(), FsTreeEntry::file("a", hash())]);
        let invalid_path = temp.path().join("invalid.jsonl");
        std::fs::write(&invalid_path, b"not a manifest\n").unwrap();

        let error = TreeMergeNewBuilder
            .build_typed(
                TreeMergeNewConfig {},
                inputs(vec![
                    ("bad", BuilderInputPath { path: invalid_path }),
                    ("valid", write_manifest(temp.path(), "valid.jsonl", &valid)),
                ]),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("input 'bad'"));
    }

    #[test]
    fn build_rejects_merge_conflicts() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let left = manifest(vec![root(), FsTreeEntry::file("a", hash())]);
        let right = manifest(vec![root(), FsTreeEntry::file("a", other_hash())]);

        let error = TreeMergeNewBuilder
            .build_typed(
                TreeMergeNewConfig {},
                inputs(vec![
                    ("left", write_manifest(temp.path(), "left.jsonl", &left)),
                    ("right", write_manifest(temp.path(), "right.jsonl", &right)),
                ]),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("conflicting fs-tree entries"));
    }

    #[test]
    fn erased_config_rejects_unknown_fields() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = TreeMergeNewBuilder
            .build_erased(
                serde_json::json!({"extra": true}),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }
}
