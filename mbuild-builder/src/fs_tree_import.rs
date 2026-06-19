use crate::{BuildContext, BuilderInputs, InputSlot, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_store::fs_tree::{FsTree, FsTreeInstall};
use mbuild_core::{BuildLogLevel, BuilderError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FsTreeImportConfig {
    pub install: FsTreeInstall,
}

pub struct FsTreeImportBuilder;

static FS_TREE_IMPORT_SPEC: InputSpec = InputSpec {
    required_inputs: &[InputSlot::object("input")],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for FsTreeImportBuilder {
    type Config = FsTreeImportConfig;

    fn tag(&self) -> &'static str {
        "FsTreeImport"
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
        let source_root = inputs.required("input")?.path.clone();
        let fs_tree = cx.fs_tree()?;
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
            object_hash: None,
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
    use crate::{Builder, BuilderInputPath};
    use bobr_store::Store;
    use bobr_store::fs_tree::{FsTreeEntry, FsTreeInstallAttrs, FsTreeInstallRule, FsTreeManifest};
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
        BuilderInputs::new(BTreeMap::from([(
            "input".to_string(),
            BuilderInputPath { path },
        )]))
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
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("tmp")).with_fs_tree(store.fs_tree());
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
        assert!(result.object_hash.is_none());
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
        let hash = manifest
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FsTreeEntry::File { path, hash } if path == "bin/tool" => Some(*hash),
                _ => None,
            })
            .expect("tool file entry");
        let hex = hash.to_hex();
        assert!(store.fs_files_dir().join(&hex[..2]).join(hex).is_file());
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
    fn build_requires_fs_tree() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let owner = fs::symlink_metadata(temp.path()).unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        fs::create_dir(cx.temp_dir.as_path()).unwrap();

        let error = FsTreeImportBuilder
            .build_typed(
                FsTreeImportConfig {
                    install: install(owner.uid(), owner.gid()),
                },
                input_object(source),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires store fs-tree operations")
        );
    }

    #[test]
    fn build_requires_input_slot() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let owner = fs::symlink_metadata(temp.path()).unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("tmp")).with_fs_tree(store.fs_tree());
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
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
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
