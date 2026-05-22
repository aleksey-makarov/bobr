//! Public ownership materialization entrypoints.

use crate::{
    error::RuntimeError,
    executor::read_executor_result_report_with_timings,
    idmap::MbuildIdmap,
    local_ownership::{preflight_local_ownership_runtime, run_local_ownership},
};
use fsobj_hash::ObjectHash;
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use std::path::Path;

/// Apply fs-tree owners and modes to an existing directory tree.
///
/// `manifest` describes paths relative to `target_root` using logical uid/gid
/// values. The runtime validates that every logical owner is representable by
/// `idmap`, starts an internal ownership helper in the mapped user namespace,
/// applies file, directory, and symlink ownership, then applies file and
/// directory modes.
///
/// `workspace` must exist and is used for temporary runtime bundle and state
/// directories. Runtime-owned temporary directories are removed before the
/// function returns.
pub fn apply_ownership_batch(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    run_ownership_batch(target_root, manifest, idmap, workspace, None)?;
    Ok(())
}

/// Apply fs-tree owners and modes, then compute the hash of a synthetic
/// fs-tree object with additional top-level non-executable metadata files.
///
/// `extra_files` contains `(name_bytes, content_bytes)` pairs. Names are fsobj
/// directory entry names, not paths; callers use this for metadata such as
/// `oci-config.json` that participates in object identity but is not part of
/// `root/`.
pub fn apply_ownership_batch_and_hash_fs_tree_object_with_extra_files(
    target_root: &Path,
    manifest: &FsTreeManifest,
    extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<ObjectHash, RuntimeError> {
    let bundle = run_ownership_batch(
        target_root,
        manifest,
        idmap,
        workspace,
        Some(HashReport::FsTreeObject {
            manifest: manifest.clone(),
            extra_files,
        }),
    )?;
    read_ownership_hash_result(bundle.result_report())
}

fn read_ownership_hash_result(path: &Path) -> Result<ObjectHash, RuntimeError> {
    let result = read_executor_result_report_with_timings(path)?.ok_or_else(|| {
        RuntimeError::Executor(format!(
            "executor result report '{}' is empty",
            path.display()
        ))
    })?;
    Ok(result.object_hash)
}

fn run_ownership_batch(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
    hash_report: Option<HashReport>,
) -> Result<crate::local_ownership::LocalOwnershipRun, RuntimeError> {
    require_directory(target_root, "ownership target root")?;
    require_directory(workspace, "ownership workspace")?;
    precheck_manifest_owners(manifest, idmap)?;
    preflight_local_ownership_runtime(idmap)?;

    run_local_ownership(target_root, manifest, idmap, workspace, hash_report)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HashReport {
    FsTreeObject {
        manifest: FsTreeManifest,
        extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    },
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

        let error = apply_ownership_batch(&missing_target, &manifest, &test_idmap(), &workspace)
            .unwrap_err();

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

        let error =
            apply_ownership_batch(&target, &manifest, &test_idmap(), &workspace_file).unwrap_err();

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
