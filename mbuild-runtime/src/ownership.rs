//! Public ownership materialization entrypoints.

use crate::{
    error::RuntimeError,
    idmap::{MbuildIdmap, cached_runtime_idmap},
    local_ownership::{preflight_local_ownership_runtime, run_local_ownership},
};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Apply fs-tree owners and modes to an existing directory tree.
///
/// `manifest` describes paths relative to `target_root` using logical uid/gid
/// values. The runtime resolves the cached host idmap internally, validates
/// that every logical owner is representable, starts an internal ownership
/// helper in the mapped user namespace, applies file, directory, and symlink
/// ownership, then applies file and directory modes.
///
/// `workspace` must exist and is used for temporary runtime bundle and state
/// directories. Runtime-owned temporary directories are removed before the
/// function returns.
pub fn apply_ownership_batch(
    target_root: &Path,
    manifest: &FsTreeManifest,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    require_directory(target_root, "ownership target root")?;
    require_directory(workspace, "ownership workspace")?;
    let idmap = cached_runtime_idmap()?;
    precheck_manifest_owners(manifest, idmap.as_ref())?;
    preflight_local_ownership_runtime(idmap.as_ref())?;

    run_local_ownership(target_root, manifest, idmap.as_ref(), workspace)
}

/// Validate that one hardlink source file matches an fs-tree file entry.
///
/// This resolves the cached host idmap internally and compares the source
/// file's physical owner and mode to the logical owner and mode in
/// `manifest_entry`.
#[cfg(unix)]
pub fn validate_fs_tree_file_attrs_in_ownership_namespace(
    source: &Path,
    manifest_entry: &FsTreeEntry,
) -> Result<(), RuntimeError> {
    let FsTreeEntry::File {
        path,
        uid,
        gid,
        mode,
        ..
    } = manifest_entry
    else {
        return Err(RuntimeError::InvalidInput(format!(
            "expected file manifest entry for '{}'",
            source.display()
        )));
    };

    let metadata = source.symlink_metadata()?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree source '{}' for '{}' must be a file",
            source.display(),
            path
        )));
    }

    let idmap = cached_runtime_idmap()?;
    let expected_uid = idmap.physical_uid(*uid).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "fs-tree entry '{}': {error}",
            entry_label(manifest_entry)
        ))
    })?;
    let expected_gid = idmap.physical_gid(*gid).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "fs-tree entry '{}': {error}",
            entry_label(manifest_entry)
        ))
    })?;
    if metadata.uid() != expected_uid || metadata.gid() != expected_gid {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree source file '{}' for '{}' has owner {}:{}, expected {}:{}",
            source.display(),
            path,
            metadata.uid(),
            metadata.gid(),
            expected_uid,
            expected_gid
        )));
    }

    let actual_mode = metadata.permissions().mode() & 0o7777;
    if actual_mode != *mode {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree source file '{}' for '{}' has mode {:o}, expected {:o}",
            source.display(),
            path,
            actual_mode,
            mode
        )));
    }

    Ok(())
}

fn entry_attrs(entry: &FsTreeEntry) -> (u32, u32, Option<u32>) {
    match entry {
        FsTreeEntry::File { uid, gid, mode, .. }
        | FsTreeEntry::Directory { uid, gid, mode, .. } => (*uid, *gid, Some(*mode)),
        FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid, None),
    }
}

fn require_directory(path: &Path, label: &str) -> Result<(), RuntimeError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "{label} '{}' must exist and be a directory",
            path.display()
        )))
    }
}

fn precheck_manifest_owners(
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    for entry in manifest.entries() {
        let (uid, gid, _) = entry_attrs(entry);
        idmap.physical_uid(uid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry_label(entry)))
        })?;
        idmap.physical_gid(gid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry_label(entry)))
        })?;
    }
    Ok(())
}

fn entry_label(entry: &FsTreeEntry) -> &str {
    if entry.path().is_empty() {
        "."
    } else {
        entry.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn require_directory_rejects_missing_and_non_directory_paths() {
        let temp = tempdir().unwrap();
        let missing = temp.path().join("missing");
        let file = temp.path().join("file");
        fs::write(&file, b"not a directory").unwrap();

        let missing_error = require_directory(&missing, "ownership target root").unwrap_err();
        let file_error = require_directory(&file, "ownership workspace").unwrap_err();

        assert!(matches!(missing_error, RuntimeError::InvalidInput(_)));
        assert!(missing_error.to_string().contains("ownership target root"));
        assert!(matches!(file_error, RuntimeError::InvalidInput(_)));
        assert!(file_error.to_string().contains("ownership workspace"));
    }

    #[test]
    fn apply_ownership_batch_rejects_invalid_target_root_before_container_setup() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let missing_target = temp.path().join("missing-target");
        let manifest = root_only_manifest();

        let error = apply_ownership_batch(&missing_target, &manifest, &workspace).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("ownership target root"));
    }

    #[test]
    fn apply_ownership_batch_rejects_invalid_workspace_before_container_setup() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let workspace_file = temp.path().join("workspace-file");
        fs::create_dir(&target).unwrap();
        fs::write(&workspace_file, b"not a directory").unwrap();
        let manifest = root_only_manifest();

        let error = apply_ownership_batch(&target, &manifest, &workspace_file).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("ownership workspace"));
    }

    #[test]
    fn precheck_manifest_owners_accepts_mapped_logical_ids() {
        let idmap = test_idmap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::file("file", 1, 1, 0o644),
            FsTreeEntry::symlink("link", 1, 1, "target"),
        ])
        .unwrap();

        precheck_manifest_owners(&manifest, &idmap).unwrap();
    }

    #[test]
    fn precheck_manifest_owners_rejects_out_of_range_uid_with_entry_path() {
        let idmap = test_idmap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::file("file", 2, 1, 0o644),
        ])
        .unwrap();

        let error = precheck_manifest_owners(&manifest, &idmap).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("fs-tree entry 'file'"));
        assert!(error.to_string().contains("logical uid 2"));
    }

    #[test]
    fn precheck_manifest_owners_rejects_out_of_range_gid_with_entry_path() {
        let idmap = test_idmap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("dir", 1, 2, 0o755),
        ])
        .unwrap();

        let error = precheck_manifest_owners(&manifest, &idmap).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("fs-tree entry 'dir'"));
        assert!(error.to_string().contains("logical gid 2"));
    }

    fn test_idmap() -> MbuildIdmap {
        MbuildIdmap::for_tests(1000, 1001, 100000, 1, 200000, 1)
    }

    fn root_only_manifest() -> FsTreeManifest {
        FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap()
    }
}
