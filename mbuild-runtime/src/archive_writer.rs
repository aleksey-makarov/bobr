use crate::{error::RuntimeError, idmap::MbuildIdmap};
use mbuild_core::{FsTreeArchiveEntrySource, FsTreeEntry, FsTreeManifest};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// A host fs-tree root used as a file-byte source for archive generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeArchiveInput {
    /// Host path to the input object's `root/` directory.
    pub root_dir: PathBuf,
}

pub(crate) fn validate_archive_request(
    kind: &str,
    inputs: &[FsTreeArchiveInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeArchiveEntrySource],
    output: &Path,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    if inputs.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree {kind} generation requires at least one input root"
        )));
    }
    if manifest.entries().len() != sources.len() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree {kind} generation source count {} does not match manifest entry count {}",
            sources.len(),
            manifest.entries().len()
        )));
    }
    for (index, input) in inputs.iter().enumerate() {
        if !input.root_dir.is_dir() {
            return Err(RuntimeError::InvalidInput(format!(
                "fs-tree {kind} input {index} root '{}' must exist and be a directory",
                input.root_dir.display()
            )));
        }
    }
    for (entry, source) in manifest.entries().iter().zip(sources) {
        match (entry, source) {
            (FsTreeEntry::Directory { .. }, FsTreeArchiveEntrySource::Directory) => {}
            (FsTreeEntry::File { .. }, FsTreeArchiveEntrySource::File { input_index, path }) => {
                if *input_index >= inputs.len() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "fs-tree {kind} source for '{}' references input index {}, but only {} input(s) exist",
                        entry.path(),
                        input_index,
                        inputs.len()
                    )));
                }
                validate_relative_path(kind, path)?;
            }
            (FsTreeEntry::Symlink { .. }, FsTreeArchiveEntrySource::Symlink) => {}
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "fs-tree {kind} source kind does not match manifest entry '{}'",
                    entry.path()
                )));
            }
        }
    }
    if !workspace.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree {kind} workspace '{}' must exist and be a directory",
            workspace.display()
        )));
    }
    let output_dir = output.parent().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "{kind} output path '{}' has no parent directory",
            output.display()
        ))
    })?;
    if !output_dir.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "{kind} output directory '{}' must exist and be a directory",
            output_dir.display()
        )));
    }
    Ok(())
}

pub(crate) fn precheck_archive_manifest_owners(
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    for entry in manifest.entries() {
        let (uid, gid) = entry_owner(entry);
        idmap.physical_uid(uid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry.path()))
        })?;
        idmap.physical_gid(gid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry.path()))
        })?;
    }
    Ok(())
}

pub(crate) fn canonicalize_input_roots(
    inputs: &[FsTreeArchiveInput],
) -> Result<Vec<PathBuf>, RuntimeError> {
    inputs
        .iter()
        .map(|input| fs::canonicalize(&input.root_dir).map_err(RuntimeError::Io))
        .collect()
}

pub(crate) fn canonicalize_output_path(path: &Path, label: &str) -> Result<PathBuf, RuntimeError> {
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

fn entry_owner(entry: &FsTreeEntry) -> (u32, u32) {
    match entry {
        FsTreeEntry::File { uid, gid, .. }
        | FsTreeEntry::Directory { uid, gid, .. }
        | FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid),
    }
}

fn validate_relative_path(kind: &str, path: &str) -> Result<(), RuntimeError> {
    if path.is_empty() || Path::new(path).is_absolute() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree {kind} source path '{path}' must be relative and non-empty"
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
                    "fs-tree {kind} source path '{path}' contains unsafe component"
                )));
            }
        }
    }
    Ok(())
}
