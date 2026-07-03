use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_core::BuildLogLevel;
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_store::fs_tree::{FsTree, FsTreeInstall};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for [`FsTreeImportBuilder`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FsTreeImportConfig {
    /// Where and how to place the imported object within the output fs-tree.
    pub install: FsTreeInstall,
}

/// Imports a content object (the `input`) into an fs-tree at a configured
/// location.
#[derive(Debug)]
pub struct FsTreeImportBuilder;

static FS_TREE_IMPORT_SPEC: InputSpec = InputSpec {
    required_inputs: &["input"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for FsTreeImportBuilder {
    type Config = FsTreeImportConfig;

    fn tag(&self) -> &'static str {
        "FsTreeImport"
    }

    fn impl_version(&self) -> &'static str {
        "1"
    }

    fn spec(&self) -> &'static InputSpec {
        &FS_TREE_IMPORT_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        let source_root = inputs.required("input")?.clone();
        let fs_tree = cx.fs_tree();
        let output_manifest = cx.temp_dir.join("fs-tree-manifest.jsonl");

        cx.log_event(
            BuildLogLevel::Info,
            "stage",
            format!(
                "importing fs-tree manifest from '{}'",
                source_root.display()
            ),
        );

        let output = cx
            .runtime()
            .run(
                &FsTreeImportFunction,
                FsTreeImportInput {
                    source_root,
                    fs_tree,
                    output_manifest: output_manifest.clone(),
                    install: config.install,
                },
            )
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;

        cx.log_event(
            BuildLogLevel::Info,
            "stage",
            format!("wrote fs-tree manifest with {} entries", output.entries),
        );

        Ok(StagedBuildResult {
            staged_path: output_manifest,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FsTreeImportFunction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FsTreeImportInput {
    source_root: PathBuf,
    fs_tree: FsTree,
    output_manifest: PathBuf,
    install: FsTreeInstall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FsTreeImportOutput {
    entries: usize,
}

impl RuntimeFunction for FsTreeImportFunction {
    type Input = FsTreeImportInput;
    type Output = FsTreeImportOutput;

    fn name(&self) -> &'static str {
        "fs-tree-import"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        let manifest = input
            .fs_tree
            .import_with_install(&input.source_root, &input.install)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let entries = manifest.entries().len();
        manifest
            .write_canonical(&input.output_manifest)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        Ok(FsTreeImportOutput { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Builder;
    use crate::test_support::store_fs_tree;
    use bobr_store::fs_tree::{FsTreeEntry, FsTreeInstallAttrs, FsTreeInstallRule, FsTreeManifest};
    use bobr_store::{Store, import_build};
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use tempfile::tempdir;

    fn install(uid: u32, gid: u32) -> FsTreeInstall {
        FsTreeInstall {
            rules: vec![FsTreeInstallRule {
                path: "**".to_string(),
                attrs: FsTreeInstallAttrs {
                    uid: Some(uid),
                    gid: Some(gid),
                    directory_mode: Some(0o755),
                    regular_file_mode: Some(0o644),
                    executable_file_mode: Some(0o755),
                },
            }],
        }
    }

    fn input_object(path: PathBuf) -> BuilderInputs {
        BuilderInputs::new(BTreeMap::from([("input".to_string(), path)]))
    }

    #[test]
    fn build_imports_tree_as_canonical_manifest() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/tool"), b"tool\n").unwrap();
        fs::set_permissions(source.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("bin/tool", source.join("tool-link")).unwrap();
        let owner = fs::symlink_metadata(temp.path()).unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"), store.fs_tree());
        fs::create_dir(cx.temp_dir.as_path()).unwrap();

        let result = FsTreeImportBuilder
            .build_typed(
                FsTreeImportConfig {
                    install: install(owner.uid(), owner.gid()),
                },
                input_object(source.clone()),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            result.staged_path,
            temp.path().join("tmp").join("fs-tree-manifest.jsonl")
        );
        let manifest = FsTreeManifest::read_canonical(&result.staged_path).unwrap();
        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "",
            owner.uid(),
            owner.gid(),
            0o755,
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "bin",
            owner.uid(),
            owner.gid(),
            0o755,
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::symlink(
            "tool-link",
            owner.uid(),
            owner.gid(),
            "bin/tool",
        )));
        assert!(manifest.entries().iter().any(|entry| matches!(
            entry,
            FsTreeEntry::File { path, .. } if path == "bin/tool"
        )));
        let manifest_hash = import_build(
            &store,
            "0".repeat(64).parse().unwrap(),
            "0".repeat(64).parse().unwrap(),
            Vec::new(),
            &result.staged_path,
            "staged-object",
        )
        .unwrap();
        let root = store
            .fs_tree()
            .ensure_materialized_root(None, manifest_hash)
            .unwrap();
        assert_eq!(fs::read(root.join("bin/tool")).unwrap(), b"tool\n");
        assert_eq!(
            fs::read_link(root.join("tool-link")).unwrap(),
            PathBuf::from("bin/tool")
        );
        assert_eq!(
            fs::symlink_metadata(source.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
    }

    #[test]
    fn build_requires_input_slot() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let owner = fs::symlink_metadata(temp.path()).unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"), store.fs_tree());
        fs::create_dir(cx.temp_dir.as_path()).unwrap();

        let error = FsTreeImportBuilder
            .build_typed(
                FsTreeImportConfig {
                    install: install(owner.uid(), owner.gid()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("required input slot 'input'"));
    }

    #[test]
    fn erased_config_rejects_legacy_symlink_mode() {
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("tmp"), store_fs_tree(temp.path()));
        let config = serde_json::json!({
            "install": {
                "rules": [{
                    "path": "**",
                    "attrs": {
                        "uid": 0,
                        "gid": 0,
                        "directory_mode": 493,
                        "regular_file_mode": 420,
                        "executable_file_mode": 493,
                        "symlink_mode": 511
                    }
                }]
            }
        });

        let error = FsTreeImportBuilder
            .build_erased(config, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("invalid builder config"));
    }
}
