use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_store::fs_tree::FsTree;
use mbuild_core::ObjectHash;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Materializes an fs-tree manifest object through `runtime` and returns the
/// store cache root path.
pub fn materialize_fs_tree_root<R>(
    runtime: &R,
    fs_tree: FsTree,
    manifest_hash: ObjectHash,
) -> Result<PathBuf, RuntimeError>
where
    R: Runtime,
{
    let output = runtime.run(
        &FsTreeMaterializeFunction,
        FsTreeMaterializeInput {
            fs_tree,
            manifest_hash,
        },
    )?;
    Ok(output.root_path)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FsTreeMaterializeFunction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FsTreeMaterializeInput {
    pub fs_tree: FsTree,
    pub manifest_hash: ObjectHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FsTreeMaterializeOutput {
    pub root_path: PathBuf,
}

impl RuntimeFunction for FsTreeMaterializeFunction {
    type Input = FsTreeMaterializeInput;
    type Output = FsTreeMaterializeOutput;

    fn name(&self) -> &'static str {
        "fs-tree-materialize"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        let root_path = input
            .fs_tree
            .ensure_materialized_root(input.manifest_hash)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        Ok(FsTreeMaterializeOutput { root_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_runtime::runtime::RuntimeFunction;
    use bobr_store::{Store, import_object};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn runtime_function_materializes_manifest_object_into_cache() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"hello\n").unwrap();

        let manifest = store.fs_tree().scan(&source).unwrap();
        let staged_manifest = temp.path().join("manifest.jsonl");
        manifest.write_canonical(&staged_manifest).unwrap();
        let manifest_hash = import_object(&store, &staged_manifest).unwrap();

        let output = FsTreeMaterializeFunction
            .call(FsTreeMaterializeInput {
                fs_tree: store.fs_tree(),
                manifest_hash,
            })
            .unwrap();

        assert_eq!(
            output.root_path,
            store.fs_trees_dir().join(manifest_hash.to_hex())
        );
        assert_eq!(fs::read(output.root_path.join("file")).unwrap(), b"hello\n");
    }
}
