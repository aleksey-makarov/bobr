//! Tar writer support for fs-tree consumers.

use crate::{
    error::RuntimeError,
    idmap::MbuildIdmap,
    local_helper::{
        preflight_local_helper_runtime, run_local_helper_with_config, write_helper_manifest,
    },
};
use mbuild_core::runtime_helper_protocol::{FsTreeArchiveEntrySource, FsTreeTarHelperConfig};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// A host fs-tree root used as a file-byte source for tar generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeTarInput {
    /// Host path to the input object's `root/` directory.
    pub root_dir: PathBuf,
}

/// The physical source selected for one entry in the output tar stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeTarEntrySource {
    /// Directory entry; metadata comes from the manifest.
    Directory,
    /// Regular file entry whose bytes are read from one input root.
    File {
        /// Index into the `FsTreeTarInput` slice.
        input_index: usize,
        /// Path relative to the selected input root.
        path: String,
    },
    /// Symlink entry; target and metadata come from the manifest.
    Symlink,
}

/// Write a deterministic tar stream for an fs-tree manifest in the ownership
/// user namespace.
///
/// `sources` must have the same length and order as `manifest.entries()`.
/// Regular file bytes are read from input roots inside the ownership user
/// namespace, while `output_tar` is created by the runtime helper.
pub fn write_fs_tree_tar_in_ownership_namespace(
    inputs: &[FsTreeTarInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeTarEntrySource],
    output_tar: &Path,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    validate_request(inputs, manifest, sources, output_tar, workspace, idmap)?;
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

fn validate_request(
    inputs: &[FsTreeTarInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeTarEntrySource],
    output_tar: &Path,
    workspace: &Path,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    if inputs.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "fs-tree tar generation requires at least one input root".to_string(),
        ));
    }
    if manifest.entries().len() != sources.len() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree tar generation source count {} does not match manifest entry count {}",
            sources.len(),
            manifest.entries().len()
        )));
    }
    for (index, input) in inputs.iter().enumerate() {
        if !input.root_dir.is_dir() {
            return Err(RuntimeError::InvalidInput(format!(
                "fs-tree tar input {index} root '{}' must exist and be a directory",
                input.root_dir.display()
            )));
        }
    }
    for entry in manifest.entries() {
        let (uid, gid) = entry_owner(entry);
        idmap.physical_uid(uid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry.path()))
        })?;
        idmap.physical_gid(gid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry.path()))
        })?;
    }
    for (entry, source) in manifest.entries().iter().zip(sources) {
        match (entry, source) {
            (FsTreeEntry::Directory { .. }, FsTreeTarEntrySource::Directory) => {}
            (FsTreeEntry::File { .. }, FsTreeTarEntrySource::File { input_index, path }) => {
                if *input_index >= inputs.len() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "fs-tree tar source for '{}' references input index {}, but only {} input(s) exist",
                        entry.path(),
                        input_index,
                        inputs.len()
                    )));
                }
                validate_relative_path(path)?;
            }
            (FsTreeEntry::Symlink { .. }, FsTreeTarEntrySource::Symlink) => {}
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "fs-tree tar source kind does not match manifest entry '{}'",
                    entry.path()
                )));
            }
        }
    }
    if !workspace.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree tar workspace '{}' must exist and be a directory",
            workspace.display()
        )));
    }
    let output_dir = output_tar.parent().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "tar output path '{}' has no parent directory",
            output_tar.display()
        ))
    })?;
    if !output_dir.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "tar output directory '{}' must exist and be a directory",
            output_dir.display()
        )));
    }
    Ok(())
}

fn tar_helper_config(
    input_roots: &[PathBuf],
    manifest_path: &Path,
    sources: &[FsTreeTarEntrySource],
    output_tar: &Path,
    error_report: &Path,
) -> Result<FsTreeTarHelperConfig, RuntimeError> {
    Ok(FsTreeTarHelperConfig {
        output_tar: output_tar.to_path_buf(),
        error_report: error_report.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
        inputs: input_roots.to_vec(),
        sources: archive_sources(sources),
    })
}

fn canonicalize_input_roots(inputs: &[FsTreeTarInput]) -> Result<Vec<PathBuf>, RuntimeError> {
    inputs
        .iter()
        .map(|input| fs::canonicalize(&input.root_dir).map_err(RuntimeError::Io))
        .collect()
}

fn canonicalize_output_path(path: &Path, label: &str) -> Result<PathBuf, RuntimeError> {
    let parent = path.parent().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "{label} '{}' has no parent directory",
            path.display()
        ))
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        RuntimeError::InvalidInput(format!("{label} '{}' has no file name", path.display()))
    })?;
    let parent = fs::canonicalize(parent)?;
    Ok(parent.join(file_name))
}

fn archive_sources(sources: &[FsTreeTarEntrySource]) -> Vec<FsTreeArchiveEntrySource> {
    sources
        .iter()
        .map(|source| match source {
            FsTreeTarEntrySource::Directory => FsTreeArchiveEntrySource::Directory,
            FsTreeTarEntrySource::File { input_index, path } => FsTreeArchiveEntrySource::File {
                input_index: *input_index,
                path: path.clone(),
            },
            FsTreeTarEntrySource::Symlink => FsTreeArchiveEntrySource::Symlink,
        })
        .collect()
}

fn entry_owner(entry: &FsTreeEntry) -> (u32, u32) {
    match entry {
        FsTreeEntry::File { uid, gid, .. }
        | FsTreeEntry::Directory { uid, gid, .. }
        | FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid),
    }
}

fn validate_relative_path(path: &str) -> Result<(), RuntimeError> {
    if path.is_empty() || Path::new(path).is_absolute() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree tar source path '{path}' must be relative and non-empty"
        )));
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "fs-tree tar source path '{path}' contains unsafe component"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let error = validate_request(
            &[FsTreeTarInput { root_dir: input }],
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
                FsTreeTarEntrySource::Directory,
                FsTreeTarEntrySource::File {
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
