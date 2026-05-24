//! Initramfs writer support for fs-tree consumers.

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
use mbuild_core::runtime_helper_protocol::FsTreeInitramfsHelperConfig;
use mbuild_core::{FsTreeArchiveEntrySource, FsTreeManifest};
use std::path::{Path, PathBuf};

/// Write a deterministic Linux `newc` initramfs for an fs-tree manifest in the
/// ownership user namespace.
///
/// `sources` must have the same length and order as `manifest.entries()`.
/// Regular file bytes are read from input roots inside the ownership user
/// namespace, while `output_initramfs` is created by the runtime helper.
pub fn write_fs_tree_initramfs_in_ownership_namespace(
    inputs: &[FsTreeArchiveInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeArchiveEntrySource],
    output_initramfs: &Path,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    validate_archive_request(
        "initramfs",
        inputs,
        manifest,
        sources,
        output_initramfs,
        workspace,
        idmap,
    )?;
    preflight_local_helper_runtime(idmap)?;
    let output_initramfs = canonicalize_output_path(output_initramfs, "initramfs output path")?;
    let input_roots = canonicalize_input_roots(inputs)?;

    run_local_helper_with_config(
        idmap,
        workspace,
        "fs-tree-initramfs",
        "fs-tree-initramfs-helper.json",
        |run_dir, error_report| {
            let manifest_path = run_dir.join("fs-tree-initramfs-manifest.jsonl");
            write_helper_manifest(&manifest_path, manifest, "fs-tree initramfs manifest")?;
            let config = initramfs_helper_config(
                &input_roots,
                &manifest_path,
                sources,
                &output_initramfs,
                error_report,
            )?;
            serde_json::to_vec(&config).map_err(|error| {
                RuntimeError::Executor(format!(
                    "failed to serialize fs-tree initramfs helper config: {error}"
                ))
            })
        },
    )
}

fn initramfs_helper_config(
    input_roots: &[PathBuf],
    manifest_path: &Path,
    sources: &[FsTreeArchiveEntrySource],
    output_initramfs: &Path,
    error_report: &Path,
) -> Result<FsTreeInitramfsHelperConfig, RuntimeError> {
    Ok(FsTreeInitramfsHelperConfig {
        output_initramfs: output_initramfs.to_path_buf(),
        error_report: error_report.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
        inputs: input_roots.to_vec(),
        sources: sources.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn initramfs_helper_config_serializes_manifest_inputs_and_sources() {
        let temp = tempdir().unwrap();
        let config = initramfs_helper_config(
            &[PathBuf::from("/input/root")],
            &temp.path().join("manifest.jsonl"),
            &[
                FsTreeArchiveEntrySource::Directory,
                FsTreeArchiveEntrySource::File {
                    input_index: 0,
                    path: "file".to_string(),
                },
            ],
            &temp.path().join("initramfs.img"),
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
