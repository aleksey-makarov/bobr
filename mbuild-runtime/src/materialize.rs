//! Fs-tree object materialization support for fs-tree consumers.

use crate::{
    archive_writer::{
        FsTreeArchiveInput, canonicalize_input_roots, canonicalize_output_path,
        precheck_archive_manifest_owners, validate_archive_request,
    },
    error::RuntimeError,
    idmap::cached_runtime_idmap,
    local_helper::{
        LocalHelperOperation, preflight_local_helper_runtime,
        run_local_helper_operation_with_result, write_helper_manifest,
    },
};
use mbuild_core::runtime_helper_protocol::{
    FsTreeMaterializeHelperConfig, FsTreeMaterializeReport, read_fs_tree_materialize_report,
};
use mbuild_core::{FsTreeArchiveEntrySource, FsTreeManifest};
use std::path::{Path, PathBuf};

/// Materialize an fs-tree object from a manifest and per-entry file sources in
/// the ownership user namespace.
///
/// `sources` must have the same length and order as `manifest.entries()`.
/// Regular files are hardlinked from input roots inside the ownership user
/// namespace. Directory and symlink metadata comes from the manifest.
pub fn materialize_fs_tree_from_sources_in_ownership_namespace(
    inputs: &[FsTreeArchiveInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeArchiveEntrySource],
    output_object_dir: &Path,
    workspace: &Path,
) -> Result<FsTreeMaterializeReport, RuntimeError> {
    validate_archive_request(
        "materialize",
        inputs,
        manifest,
        sources,
        output_object_dir,
        workspace,
    )?;
    if output_object_dir.exists() || output_object_dir.is_symlink() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree materialize output object directory '{}' must not already exist",
            output_object_dir.display()
        )));
    }

    let idmap = cached_runtime_idmap()?;
    precheck_archive_manifest_owners(manifest, idmap.as_ref())?;
    preflight_local_helper_runtime(idmap.as_ref())?;
    let output_object_dir = canonicalize_output_path(
        output_object_dir,
        "fs-tree materialize output object directory",
    )?;
    let input_roots = canonicalize_input_roots(inputs)?;

    run_local_helper_operation_with_result(
        idmap.as_ref(),
        workspace,
        FsTreeMaterializeOperation {
            input_roots,
            manifest,
            sources,
            output_object_dir,
        },
        |run_dir| {
            read_fs_tree_materialize_report(&run_dir.join("success.json")).map_err(|error| {
                RuntimeError::Executor(format!(
                    "failed to read fs-tree materialize success report: {error}"
                ))
            })
        },
    )
}

struct FsTreeMaterializeOperation<'a> {
    input_roots: Vec<PathBuf>,
    manifest: &'a FsTreeManifest,
    sources: &'a [FsTreeArchiveEntrySource],
    output_object_dir: PathBuf,
}

impl LocalHelperOperation for FsTreeMaterializeOperation<'_> {
    type Config = FsTreeMaterializeHelperConfig;

    const COMMAND: &'static str = "fs-tree-materialize";
    const CONFIG_FILE: &'static str = "fs-tree-materialize-helper.json";
    const CONFIG_LABEL: &'static str = "fs-tree materialize helper config";

    fn build_config(
        &self,
        run_dir: &Path,
        error_report: &Path,
    ) -> Result<Self::Config, RuntimeError> {
        let manifest_path = run_dir.join("fs-tree-materialize-manifest.jsonl");
        write_helper_manifest(
            &manifest_path,
            self.manifest,
            "fs-tree materialize manifest",
        )?;
        Ok(FsTreeMaterializeHelperConfig {
            output_object_dir: self.output_object_dir.clone(),
            error_report: error_report.to_path_buf(),
            success_report: run_dir.join("success.json"),
            manifest_path,
            inputs: self.input_roots.clone(),
            sources: self.sources.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::FsTreeEntry;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn materialize_operation_builds_config_with_success_report() {
        let temp = tempdir().unwrap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::file("file", 1, 1, 0o644),
        ])
        .unwrap();
        let sources = [
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::File {
                input_index: 0,
                path: "file".to_string(),
            },
        ];
        let operation = FsTreeMaterializeOperation {
            input_roots: vec![PathBuf::from("/input/root")],
            manifest: &manifest,
            sources: &sources,
            output_object_dir: temp.path().join("out.obj"),
        };
        let config = operation
            .build_config(temp.path(), &temp.path().join("error.json"))
            .unwrap();

        assert_eq!(
            config.manifest_path,
            temp.path().join("fs-tree-materialize-manifest.jsonl")
        );
        assert!(config.manifest_path.is_file());
        assert_eq!(config.success_report, temp.path().join("success.json"));
        assert_eq!(config.inputs[0], PathBuf::from("/input/root"));
        assert_eq!(
            config.sources[1],
            FsTreeArchiveEntrySource::File {
                input_index: 0,
                path: "file".to_string(),
            }
        );
    }

    #[test]
    fn materialize_facade_rejects_existing_output_before_helper_setup() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        let workspace = temp.path().join("workspace");
        let output = temp.path().join("output.obj");
        fs::create_dir(&input).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&output).unwrap();
        let manifest =
            FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap();

        let error = materialize_fs_tree_from_sources_in_ownership_namespace(
            &[FsTreeArchiveInput { root_dir: input }],
            &manifest,
            &[FsTreeArchiveEntrySource::Directory],
            &output,
            &workspace,
        )
        .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("must not already exist"));
    }
}
