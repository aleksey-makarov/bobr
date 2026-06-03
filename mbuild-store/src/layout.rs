use crate::StoreError;
use crate::fsutil as private_fs;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const OBJECTS_DIR: &str = "objects";
pub(crate) const BUILDS_DIR: &str = "builds";
pub(crate) const RESULTS_DIR: &str = "results";
pub(crate) const REUSES_DIR: &str = "reuses";
pub(crate) const OBJECT_REFS_DIR: &str = "object-refs";
pub(crate) const RESULT_REFS_DIR: &str = "result-refs";
pub(crate) const FS_FILES_DIR: &str = "fs-files";
pub(crate) const FS_TREES_DIR: &str = "fs-trees";

/// Resolved directory layout of an `mbuild` store.
///
/// A layout is created by [`StoreLayout::discover`] or
/// [`StoreLayout::discover_in_cwd`]. Discovery creates the known store
/// directories if they do not already exist and then returns paths rooted at
/// the directory supplied by the caller.
#[derive(Debug, Clone)]
pub struct StoreLayout {
    /// Store root directory.
    pub root: PathBuf,
    /// Content-addressed legacy object directory.
    pub objects: PathBuf,
    /// Build-key references pointing at result records.
    pub builds: PathBuf,
    /// Reuse-key references pointing at result records.
    pub reuses: PathBuf,
    /// JSON result records keyed by [`crate::ResultId`].
    pub results: PathBuf,
    /// Public object references keyed by output name.
    pub object_refs: PathBuf,
    /// Public result references keyed by output name.
    pub result_refs: PathBuf,
    /// Content-addressed future fs-file object directory.
    pub fs_files: PathBuf,
    /// Future fs-tree manifest directory.
    pub fs_trees: PathBuf,
}

impl StoreLayout {
    /// Discovers or initializes a store rooted at `root`.
    ///
    /// The function creates every directory known to the current store layout:
    /// legacy objects, build and reuse refs, result records, public refs, and
    /// future `fs-files`/`fs-trees` directories. It does not validate existing
    /// records or references.
    pub fn discover(root: &Path) -> Result<Self, StoreError> {
        let layout = Self {
            root: root.to_path_buf(),
            objects: root.join(OBJECTS_DIR),
            builds: root.join(BUILDS_DIR),
            reuses: root.join(REUSES_DIR),
            results: root.join(RESULTS_DIR),
            object_refs: root.join(OBJECT_REFS_DIR),
            result_refs: root.join(RESULT_REFS_DIR),
            fs_files: root.join(FS_FILES_DIR),
            fs_trees: root.join(FS_TREES_DIR),
        };
        layout.ensure()?;
        Ok(layout)
    }

    /// Discovers or initializes an `mbuild` store in the current directory.
    ///
    /// This is a convenience wrapper around [`StoreLayout::discover`] using
    /// [`std::env::current_dir`].
    pub fn discover_in_cwd() -> Result<Self, StoreError> {
        let cwd = env::current_dir()
            .map_err(|error| StoreError::Io(format!("failed to get current directory: {error}")))?;
        Self::discover(&cwd)
    }

    fn ensure(&self) -> Result<(), StoreError> {
        ensure_dir(&self.root, "mbuild root")?;
        ensure_dir(&self.objects, "objects")?;
        ensure_dir(&self.builds, "builds")?;
        ensure_dir(&self.reuses, "reuses")?;
        ensure_dir(&self.results, "results")?;
        ensure_dir(&self.object_refs, "object-refs")?;
        ensure_dir(&self.result_refs, "result-refs")?;
        ensure_dir(&self.fs_files, "fs-files")?;
        ensure_dir(&self.fs_trees, "fs-trees")?;
        Ok(())
    }
}

fn ensure_dir(path: &Path, label: &str) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|error| {
        StoreError::Io(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

/// Removes and recreates a store-owned temporary directory.
///
/// The directory must be below [`StoreLayout::root`] and include a `tmp` path
/// component. This guard keeps force-removal scoped to temporary directories
/// that belong to the store. The resulting directory exists and is empty.
pub fn recreate_store_temp_dir_force(
    layout: &StoreLayout,
    temp_dir: &Path,
) -> Result<(), StoreError> {
    validate_store_temp_dir(layout, temp_dir)?;
    private_fs::recreate_empty_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

/// Removes a store-owned temporary directory if it exists.
///
/// The directory must be below [`StoreLayout::root`] and include a `tmp` path
/// component. Missing directories are treated as success.
pub fn remove_store_temp_dir_force(
    layout: &StoreLayout,
    temp_dir: &Path,
) -> Result<(), StoreError> {
    validate_store_temp_dir(layout, temp_dir)?;
    private_fs::remove_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

fn validate_store_temp_dir(layout: &StoreLayout, temp_dir: &Path) -> Result<(), StoreError> {
    if temp_dir == layout.root || !temp_dir.starts_with(&layout.root) {
        return Err(StoreError::InvalidInput(format!(
            "store temp directory '{}' must be under store root '{}'",
            temp_dir.display(),
            layout.root.display()
        )));
    }

    if !temp_dir
        .components()
        .any(|component| component.as_os_str() == OsStr::new("tmp"))
    {
        return Err(StoreError::InvalidInput(format!(
            "store temp directory '{}' must include a 'tmp' path component",
            temp_dir.display()
        )));
    }

    Ok(())
}
