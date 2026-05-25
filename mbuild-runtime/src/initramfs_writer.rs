//! Initramfs writer support for fs-tree consumers.

use crate::{
    archive_writer::{
        FsTreeArchiveInput, canonicalize_input_roots, canonicalize_output_path,
        validate_archive_request,
    },
    error::RuntimeError,
    idmap::MbuildIdmap,
    local_helper::{
        LocalHelperOperation, preflight_local_helper_runtime, run_local_helper_operation,
        write_helper_manifest,
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

    run_local_helper_operation(
        idmap,
        workspace,
        FsTreeInitramfsOperation {
            input_roots,
            manifest,
            sources,
            output_initramfs,
        },
    )
}

struct FsTreeInitramfsOperation<'a> {
    input_roots: Vec<PathBuf>,
    manifest: &'a FsTreeManifest,
    sources: &'a [FsTreeArchiveEntrySource],
    output_initramfs: PathBuf,
}

impl LocalHelperOperation for FsTreeInitramfsOperation<'_> {
    type Config = FsTreeInitramfsHelperConfig;

    const COMMAND: &'static str = "fs-tree-initramfs";
    const CONFIG_FILE: &'static str = "fs-tree-initramfs-helper.json";
    const CONFIG_LABEL: &'static str = "fs-tree initramfs helper config";

    fn build_config(
        &self,
        run_dir: &Path,
        error_report: &Path,
    ) -> Result<Self::Config, RuntimeError> {
        let manifest_path = run_dir.join("fs-tree-initramfs-manifest.jsonl");
        write_helper_manifest(&manifest_path, self.manifest, "fs-tree initramfs manifest")?;
        Ok(FsTreeInitramfsHelperConfig {
            output_initramfs: self.output_initramfs.clone(),
            error_report: error_report.to_path_buf(),
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
    use tempfile::tempdir;

    #[test]
    fn initramfs_operation_builds_config_with_manifest_inputs_and_sources() {
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
        let operation = FsTreeInitramfsOperation {
            input_roots: vec![PathBuf::from("/input/root")],
            manifest: &manifest,
            sources: &sources,
            output_initramfs: temp.path().join("initramfs.img"),
        };
        let config = operation
            .build_config(temp.path(), &temp.path().join("error.json"))
            .unwrap();

        assert_eq!(
            config.manifest_path,
            temp.path().join("fs-tree-initramfs-manifest.jsonl")
        );
        assert!(config.manifest_path.is_file());
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
