use crate::StoreError;
use crate::fsutil as private_fs;
use crate::{BuildKey, ResultId, ReuseKey};
use fsobj_hash::ObjectHash;
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

/// Immutable handle to an `mbuild` store.
///
/// `Store` is the primary public interface for paths and operations that belong
/// to a store. It stores only the root path; all layout paths are derived from
/// the root through methods, so callers cannot mutate the store layout fields.
///
/// A `Store` is cloneable and thread-safe. Cloning copies the root path handle,
/// not store data.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Creates or initializes a store layout under an existing root directory.
    ///
    /// `root` must be an absolute path to an existing directory. Missing store
    /// subdirectories are created. Existing store subdirectories must be
    /// directories. The function does not validate existing records or
    /// references inside those directories.
    pub fn create(root: &Path) -> Result<Self, StoreError> {
        validate_root(root)?;
        let store = Self {
            root: root.to_path_buf(),
        };
        store.ensure()?;
        Ok(store)
    }

    /// Returns the store root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the content-addressed legacy object directory.
    pub(crate) fn objects_dir(&self) -> PathBuf {
        self.root.join(OBJECTS_DIR)
    }

    /// Returns the build-key reference directory.
    pub(crate) fn builds_dir(&self) -> PathBuf {
        self.root.join(BUILDS_DIR)
    }

    /// Returns the reuse-key reference directory.
    pub(crate) fn reuses_dir(&self) -> PathBuf {
        self.root.join(REUSES_DIR)
    }

    /// Returns the JSON result record directory.
    pub(crate) fn results_dir(&self) -> PathBuf {
        self.root.join(RESULTS_DIR)
    }

    /// Returns the public object reference directory.
    pub(crate) fn object_refs_dir(&self) -> PathBuf {
        self.root.join(OBJECT_REFS_DIR)
    }

    /// Returns the public result reference directory.
    pub(crate) fn result_refs_dir(&self) -> PathBuf {
        self.root.join(RESULT_REFS_DIR)
    }

    /// Returns the content-addressed future fs-file object directory.
    pub fn fs_files_dir(&self) -> PathBuf {
        self.root.join(FS_FILES_DIR)
    }

    /// Returns the future fs-tree manifest directory.
    pub fn fs_trees_dir(&self) -> PathBuf {
        self.root.join(FS_TREES_DIR)
    }

    /// Returns the canonical path of an imported legacy object.
    ///
    /// The path is `<store>/objects/<64-lowercase-object-hash>`. The function
    /// does not check whether the object currently exists.
    pub fn object_path(&self, object_hash: ObjectHash) -> PathBuf {
        self.objects_dir().join(object_hash.to_hex())
    }

    /// Returns the path of the build reference for `build_key`.
    ///
    /// The path is under the build-key reference directory and may or may not exist.
    pub(crate) fn build_ref_path(&self, build_key: BuildKey) -> PathBuf {
        self.builds_dir().join(build_key.to_hex())
    }

    /// Returns the path of the reuse reference for `reuse_key`.
    ///
    /// The path is under the reuse-key reference directory and may or may not exist.
    pub(crate) fn reuse_ref_path(&self, reuse_key: ReuseKey) -> PathBuf {
        self.reuses_dir().join(reuse_key.to_hex())
    }

    /// Returns the path of the JSON result record for `result_id`.
    ///
    /// The path is under the JSON result record directory and has a `.json` suffix. The
    /// function does not check whether the record currently exists.
    pub(crate) fn result_record_path(&self, result_id: ResultId) -> PathBuf {
        self.results_dir()
            .join(format!("{}.json", result_id.to_hex()))
    }

    fn ensure(&self) -> Result<(), StoreError> {
        ensure_store_dir(&self.objects_dir(), "objects")?;
        ensure_store_dir(&self.builds_dir(), "builds")?;
        ensure_store_dir(&self.reuses_dir(), "reuses")?;
        ensure_store_dir(&self.results_dir(), "results")?;
        ensure_store_dir(&self.object_refs_dir(), "object-refs")?;
        ensure_store_dir(&self.result_refs_dir(), "result-refs")?;
        ensure_store_dir(&self.fs_files_dir(), "fs-files")?;
        ensure_store_dir(&self.fs_trees_dir(), "fs-trees")?;
        Ok(())
    }
}

fn validate_root(root: &Path) -> Result<(), StoreError> {
    if !root.is_absolute() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be absolute: '{}'",
            root.display()
        )));
    }
    let metadata = fs::metadata(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StoreError::InvalidInput(format!("store root must exist: '{}'", root.display()))
        } else {
            StoreError::Io(format!(
                "failed to inspect store root '{}': {error}",
                root.display()
            ))
        }
    })?;
    if !metadata.is_dir() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be a directory: '{}'",
            root.display()
        )));
    }
    Ok(())
}

fn ensure_store_dir(path: &Path, label: &str) -> Result<(), StoreError> {
    if path.exists() || path.is_symlink() {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            StoreError::Io(format!(
                "failed to inspect {label} directory '{}': {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_dir() {
            return Ok(());
        }
        return Err(StoreError::InvalidData(format!(
            "store {label} path '{}' is not a directory",
            path.display()
        )));
    }

    fs::create_dir_all(path).map_err(|error| {
        StoreError::Io(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

/// Removes and recreates a store-owned temporary directory.
///
/// The directory must be below [`Store::root`] and include a `tmp` path
/// component. This guard keeps force-removal scoped to temporary directories
/// that belong to the store. The resulting directory exists and is empty.
pub fn recreate_store_temp_dir_force(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    validate_store_temp_dir(store, temp_dir)?;
    private_fs::recreate_empty_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

/// Removes a store-owned temporary directory if it exists.
///
/// The directory must be below [`Store::root`] and include a `tmp` path
/// component. Missing directories are treated as success.
pub fn remove_store_temp_dir_force(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    validate_store_temp_dir(store, temp_dir)?;
    private_fs::remove_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

fn validate_store_temp_dir(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    if temp_dir == store.root() || !temp_dir.starts_with(store.root()) {
        return Err(StoreError::InvalidInput(format!(
            "store temp directory '{}' must be under store root '{}'",
            temp_dir.display(),
            store.root().display()
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
