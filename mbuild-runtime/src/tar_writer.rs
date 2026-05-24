//! Tar writer support for fs-tree consumers.

use crate::{
    archive_writer::{
        FsTreeArchiveInput, canonicalize_input_roots, canonicalize_output_path,
        validate_archive_request,
    },
    error::RuntimeError,
    idmap::MbuildIdmap,
    local_helper::{
        preflight_local_helper_runtime, run_local_helper_with_config, write_helper_manifest,
    },
};
use mbuild_core::runtime_helper_protocol::FsTreeTarHelperConfig;
use mbuild_core::{FsTreeArchiveEntrySource, FsTreeManifest};
use std::path::{Path, PathBuf};

/// Write a deterministic tar stream for an fs-tree manifest in the ownership
/// user namespace.
///
/// `sources` must have the same length and order as `manifest.entries()`.
/// Regular file bytes are read from input roots inside the ownership user
/// namespace, while `output_tar` is created by the runtime helper.
pub fn write_fs_tree_tar_in_ownership_namespace(
    inputs: &[FsTreeArchiveInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeArchiveEntrySource],
    output_tar: &Path,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    validate_archive_request(
        "tar", inputs, manifest, sources, output_tar, workspace, idmap,
    )?;
    preflight_local_helper_runtime(idmap)?;
    let output_tar = canonicalize_output_path(output_tar, "tar output path")?;
    let input_roots = canonicalize_input_roots(inputs)?;

    run_local_helper_with_config(
        idmap,
        workspace,
        "fs-tree-tar",
        "fs-tree-tar-helper.json",
        |run_dir, error_report| {
            let manifest_path = run_dir.join("fs-tree-tar-manifest.jsonl");
            write_helper_manifest(&manifest_path, manifest, "fs-tree tar manifest")?;
            let config = tar_helper_config(
                &input_roots,
                &manifest_path,
                sources,
                &output_tar,
                error_report,
            )?;
            serde_json::to_vec(&config).map_err(|error| {
                RuntimeError::Executor(format!(
                    "failed to serialize fs-tree tar helper config: {error}"
                ))
            })
        },
    )
}

fn tar_helper_config(
    input_roots: &[PathBuf],
    manifest_path: &Path,
    sources: &[FsTreeArchiveEntrySource],
    output_tar: &Path,
    error_report: &Path,
) -> Result<FsTreeTarHelperConfig, RuntimeError> {
    Ok(FsTreeTarHelperConfig {
        output_tar: output_tar.to_path_buf(),
        error_report: error_report.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
        inputs: input_roots.to_vec(),
        sources: sources.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::FsTreeEntry;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn validate_request_rejects_source_count_mismatch() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        fs::create_dir(&input).unwrap();
        let output = temp.path().join("out.tar");
        let manifest =
            FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap();
        let idmap = MbuildIdmap::for_tests(1000, 1000, 100000, 10, 200000, 10);

        let error = validate_archive_request(
            "tar",
            &[FsTreeArchiveInput { root_dir: input }],
            &manifest,
            &[],
            &output,
            temp.path(),
            &idmap,
        )
        .unwrap_err();

        assert!(error.to_string().contains("source count"));
    }

    #[test]
    fn tar_helper_config_serializes_manifest_inputs_and_sources() {
        let temp = tempdir().unwrap();
        let config = tar_helper_config(
            &[PathBuf::from("/input/root")],
            &temp.path().join("manifest.jsonl"),
            &[
                FsTreeArchiveEntrySource::Directory,
                FsTreeArchiveEntrySource::File {
                    input_index: 0,
                    path: "file".to_string(),
                },
            ],
            &temp.path().join("out.tar"),
            &temp.path().join("error.json"),
        )
        .unwrap();

        assert_eq!(config.manifest_path, temp.path().join("manifest.jsonl"));
        assert_eq!(config.inputs[0], PathBuf::from("/input/root"));
        assert_eq!(
            config.sources[1],
            FsTreeArchiveEntrySource::File {
                input_index: 0,
                path: "file".to_string(),
            }
        );
    }
}
