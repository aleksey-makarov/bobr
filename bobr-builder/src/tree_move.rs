use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSpec, TypedBuilder};
use bobr_core::BuildLogLevel;
use bobr_store::{
    StoreError,
    fs_tree::{FsTreeManifest, strip_prefix_manifest},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Builds an fs-tree that re-roots its input `tree` at a subdirectory: the
/// `strip_prefix` directory becomes the new root and its leading path component
/// is removed from every nested entry. A pure manifest operation — the same
/// fs-files blobs are referenced under shorter paths.
#[derive(Debug)]
pub struct TreeMoveBuilder;

/// Configuration for [`TreeMoveBuilder`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreeMoveConfig {
    strip_prefix: String,
}

static TREE_MOVE_SPEC: InputSpec = InputSpec {
    required_inputs: &["tree"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for TreeMoveBuilder {
    type Config = TreeMoveConfig;

    fn tag(&self) -> &'static str {
        "TreeMove"
    }

    fn impl_version(&self) -> &'static str {
        "1"
    }

    fn spec(&self) -> &'static InputSpec {
        &TREE_MOVE_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError> {
        build_tree_move(config, inputs, cx)
    }
}

fn build_tree_move(
    config: TreeMoveConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<PathBuf, BuilderError> {
    let input = inputs.required("tree")?;
    let manifest = FsTreeManifest::read_canonical(input).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "TreeMove input 'tree' is not a valid fs-tree manifest: {error}"
        ))
    })?;

    cx.log_event(
        BuildLogLevel::Info,
        "move",
        format!("re-rooting fs-tree manifest at '{}'", config.strip_prefix),
    );

    let moved = strip_prefix_manifest(&manifest, &config.strip_prefix).map_err(map_move_error)?;
    let output_path = cx.temp_dir.join("fs-tree-move-manifest.jsonl");
    moved.write_canonical(&output_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to write fs-tree move manifest '{}': {error}",
            output_path.display()
        ))
    })?;

    cx.log_event(
        BuildLogLevel::Info,
        "move",
        format!(
            "wrote fs-tree move manifest with {} entries",
            moved.entries().len()
        ),
    );

    Ok(output_path)
}

fn map_move_error(error: StoreError) -> BuilderError {
    match error {
        StoreError::InvalidInput(message) => BuilderError::InvalidRecipe(message),
        error => BuilderError::ExecutionFailed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Builder;
    use crate::test_support::store_fs_tree;
    use bobr_store::fs_tree::{FsFileHash, FsTreeEntry};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn hash() -> FsFileHash {
        FsFileHash::from_str("1111111111111111111111111111111111111111111111111111111111111111")
            .unwrap()
    }

    fn manifest(entries: Vec<FsTreeEntry>) -> FsTreeManifest {
        FsTreeManifest::from_entries(entries).unwrap()
    }

    fn write_manifest(root: &std::path::Path, name: &str, manifest: &FsTreeManifest) -> PathBuf {
        let path = root.join(name);
        manifest.write_canonical(&path).unwrap();
        path
    }

    fn inputs(entries: Vec<(&str, PathBuf)>) -> BuilderInputs {
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

    fn build(
        config: TreeMoveConfig,
        input: &FsTreeManifest,
    ) -> Result<FsTreeManifest, BuilderError> {
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("tmp"), store_fs_tree(temp.path()));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let result = TreeMoveBuilder.build_typed(
            config,
            inputs(vec![(
                "tree",
                write_manifest(temp.path(), "tree.jsonl", input),
            )]),
            &mut cx,
        )?;
        Ok(FsTreeManifest::read_canonical(&result).unwrap())
    }

    fn paths(manifest: &FsTreeManifest) -> Vec<&str> {
        manifest.entries().iter().map(FsTreeEntry::path).collect()
    }

    #[test]
    fn spec_requires_tree_input_only() {
        assert_eq!(TypedBuilder::tag(&TreeMoveBuilder), "TreeMove");
        assert_eq!(TREE_MOVE_SPEC.required_inputs, &["tree"]);
        assert!(TREE_MOVE_SPEC.optional_inputs.is_empty());
        assert!(!TREE_MOVE_SPEC.allow_extra_inputs);
    }

    #[test]
    fn reroots_at_single_component_prefix() {
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("stage", 0, 0, 0o755),
            FsTreeEntry::directory("stage/usr", 0, 0, 0o755),
            FsTreeEntry::directory("stage/usr/bin", 0, 0, 0o755),
            FsTreeEntry::file("stage/usr/bin/tool", hash()),
            FsTreeEntry::symlink("stage/usr/sbin", 0, 0, "bin"),
        ]);
        let moved = build(
            TreeMoveConfig {
                strip_prefix: "stage".to_string(),
            },
            &input,
        )
        .unwrap();
        assert_eq!(
            paths(&moved),
            vec!["", "usr", "usr/bin", "usr/bin/tool", "usr/sbin"]
        );
        // Payload is preserved (same fs-file hash under the shorter path).
        let file = moved
            .entries()
            .iter()
            .find(|entry| entry.path() == "usr/bin/tool")
            .unwrap();
        assert!(matches!(file, FsTreeEntry::File { hash: h, .. } if *h == hash()));
    }

    #[test]
    fn reroots_at_multi_component_prefix() {
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("build", 0, 0, 0o755),
            FsTreeEntry::directory("build/stage", 0, 0, 0o755),
            FsTreeEntry::directory("build/stage/usr", 0, 0, 0o755),
            FsTreeEntry::file("build/stage/usr/lib.so", hash()),
        ]);
        let moved = build(
            TreeMoveConfig {
                strip_prefix: "build/stage".to_string(),
            },
            &input,
        )
        .unwrap();
        assert_eq!(paths(&moved), vec!["", "usr", "usr/lib.so"]);
    }

    #[test]
    fn rejects_entry_outside_prefix() {
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("stage", 0, 0, 0o755),
            FsTreeEntry::file("stage/keep", hash()),
            // Litter: a build that wrote outside its stage.
            FsTreeEntry::directory("etc", 0, 0, 0o755),
            FsTreeEntry::file("etc/stray", hash()),
        ]);
        let error = build(
            TreeMoveConfig {
                strip_prefix: "stage".to_string(),
            },
            &input,
        )
        .unwrap_err();
        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
        assert!(error.to_string().contains("outside strip_prefix"));
        assert!(error.to_string().contains("etc"));
    }

    #[test]
    fn rejects_prefix_that_matches_nothing() {
        let input = manifest(vec![root(), FsTreeEntry::directory("stage", 0, 0, 0o755)]);
        let error = build(
            TreeMoveConfig {
                strip_prefix: "stage".to_string(),
            },
            &input,
        )
        .unwrap_err();
        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
        assert!(error.to_string().contains("selected no paths"));
    }

    #[test]
    fn rejects_absent_prefix() {
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::file("usr/tool", hash()),
        ]);
        let error = build(
            TreeMoveConfig {
                strip_prefix: "stage".to_string(),
            },
            &input,
        )
        .unwrap_err();
        // "usr/tool" is outside "stage" -> litter error.
        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn rejects_malformed_prefix() {
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("stage", 0, 0, 0o755),
            FsTreeEntry::file("stage/tool", hash()),
        ]);
        for bad in ["", "/stage", "stage/", "sta//ge", "./stage", "../stage"] {
            let error = build(
                TreeMoveConfig {
                    strip_prefix: bad.to_string(),
                },
                &input,
            )
            .unwrap_err();
            assert!(
                matches!(error, BuilderError::InvalidRecipe(_)),
                "prefix {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn build_reports_tree_input_for_invalid_manifest() {
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("tmp"), store_fs_tree(temp.path()));
        std::fs::create_dir(&cx.temp_dir).unwrap();
        let invalid_path = temp.path().join("invalid.jsonl");
        std::fs::write(&invalid_path, b"not a manifest\n").unwrap();

        let error = TreeMoveBuilder
            .build_typed(
                TreeMoveConfig {
                    strip_prefix: "stage".to_string(),
                },
                inputs(vec![("tree", invalid_path)]),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("input 'tree'"));
    }

    #[test]
    fn plan_rejects_unknown_config_fields() {
        static BUILDER: TreeMoveBuilder = TreeMoveBuilder;
        let error = BUILDER
            .plan(serde_json::json!({"strip_prefix": "stage", "extra": true}))
            .err()
            .expect("plan should reject unknown config fields");
        assert!(error.to_string().contains("unknown field"));
    }
}
