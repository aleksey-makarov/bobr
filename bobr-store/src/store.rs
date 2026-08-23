use crate::StoreError;
use crate::fs_tree::{FsFileHash, FsTree, FsTreeEntry, read_manifest_if_marked};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const OBJECTS_DIR: &str = "objects";
pub(crate) const BUILDS_DIR: &str = "builds";
pub(crate) const OBJECT_RECORDS_DIR: &str = "object-records";
pub(crate) const REUSES_DIR: &str = "reuses";
pub(crate) const OBJECT_REFS_DIR: &str = "object-refs";
pub(crate) const FS_FILES_DIR: &str = "fs-files";
pub(crate) const FS_TREES_DIR: &str = "fs-trees";
pub(crate) const FS_TREE_REFS_DIR: &str = "fs-tree-refs";

/// Immutable handle to a `bobr` store.
///
/// `Store` is the primary public interface for paths and operations that belong
/// to a store: objects, their records, the reuse and name references, and the
/// filesystem trees. It knows nothing about where a particular build writes its
/// logs and scratch -- that belongs to one run, not to the store.
///
/// A `Store` is cloneable and thread-safe; clones share the same root.
#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
}

#[derive(Debug)]
struct StoreInner {
    root: PathBuf,
}

/// Read-only handle to an existing `bobr` store layout.
///
/// Unlike [`Store::create`], opening this handle never creates or repairs
/// directories. The type exposes no mutation operations; secondary-store
/// capabilities wrap it to read trusted indexes or locate content without
/// accidentally treating the secondary as the working store.
#[derive(Debug, Clone)]
pub struct ReadOnlyStore {
    inner: Arc<ReadOnlyStoreInner>,
}

#[derive(Debug)]
struct ReadOnlyStoreInner {
    root: PathBuf,
}

impl Store {
    /// Creates or initializes a store layout under an existing root directory.
    ///
    /// `root` must be an absolute path to an existing directory. Missing store
    /// subdirectories are created. Existing store subdirectories must be
    /// directories. The function does not validate existing records or
    /// references inside those directories. Symlink roots are accepted and
    /// resolved to their canonical target for the lifetime of the returned
    /// handle.
    pub fn create(root: &Path) -> Result<Self, StoreError> {
        let root = validate_root(root)?;
        ensure_store_layout(&root)?;
        Ok(Self {
            inner: Arc::new(StoreInner { root }),
        })
    }

    /// Returns the canonical store root.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Returns store-scoped fs-tree operations.
    pub fn fs_tree(&self) -> FsTree {
        FsTree::new(self.root().to_path_buf())
    }

    /// Returns the content-addressed legacy object directory.
    pub(crate) fn objects_dir(&self) -> PathBuf {
        self.root().join(OBJECTS_DIR)
    }

    /// Returns the build-key reference directory.
    pub(crate) fn builds_dir(&self) -> PathBuf {
        self.root().join(BUILDS_DIR)
    }

    /// Returns the reuse-key reference directory.
    pub(crate) fn reuses_dir(&self) -> PathBuf {
        self.root().join(REUSES_DIR)
    }

    /// Returns the JSON object record directory.
    pub(crate) fn object_records_dir(&self) -> PathBuf {
        self.root().join(OBJECT_RECORDS_DIR)
    }

    /// Returns the public object reference directory.
    pub(crate) fn object_refs_dir(&self) -> PathBuf {
        self.root().join(OBJECT_REFS_DIR)
    }

    /// Returns the path of an existing imported object, or `None` when it is
    /// absent from the store.
    ///
    /// The path is `<store>/objects/<64-lowercase-object-hash>`.
    pub fn object_path(&self, object_hash: ObjectHash) -> Result<Option<PathBuf>, StoreError> {
        let path = self.object_path_unchecked(object_hash);
        match path.try_exists() {
            Ok(true) => Ok(Some(path)),
            Ok(false) => Ok(None),
            Err(error) => Err(StoreError::Io(format!(
                "failed to check object '{object_hash}' at '{}': {error}",
                path.display()
            ))),
        }
    }

    /// Returns whether the complete content-addressed object is present.
    ///
    /// Ordinary files and directories are complete when their top-level object
    /// path exists with the expected file type. An fs-tree manifest is complete
    /// only when every referenced fs-file is present as a regular file.
    /// Object records are deliberately not consulted.
    pub fn object_is_complete(&self, object_hash: ObjectHash) -> Result<bool, StoreError> {
        let Some(path) = self.object_path(object_hash)? else {
            return Ok(false);
        };
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            StoreError::Io(format!(
                "failed to inspect working object '{}': {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_dir() {
            return Ok(true);
        }
        if !metadata.file_type().is_file() {
            return Err(StoreError::InvalidData(format!(
                "working object path '{}' is neither a regular file nor a directory",
                path.display()
            )));
        }
        let Some(manifest) = read_manifest_if_marked(&path)? else {
            return Ok(true);
        };
        let mut seen = std::collections::HashSet::new();
        for file_hash in manifest.entries().iter().filter_map(|entry| match entry {
            FsTreeEntry::File { hash, .. } if seen.insert(*hash) => Some(*hash),
            _ => None,
        }) {
            let fs_file = self.fs_file_path_unchecked(file_hash);
            match fs::symlink_metadata(&fs_file) {
                Ok(metadata) if metadata.file_type().is_file() => {}
                Ok(_) => {
                    return Err(StoreError::InvalidData(format!(
                        "working fs-file path '{}' is not a regular file",
                        fs_file.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(StoreError::Io(format!(
                        "failed to inspect working fs-file '{}': {error}",
                        fs_file.display()
                    )));
                }
            }
        }
        Ok(true)
    }

    /// Returns the canonical path of an imported object without checking that it
    /// exists. The path is `<store>/objects/<64-lowercase-object-hash>`.
    pub(crate) fn object_path_unchecked(&self, object_hash: ObjectHash) -> PathBuf {
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

    /// Returns the path of the JSON object record for `object_hash`.
    ///
    /// The path is under the JSON object record directory and has a `.json`
    /// suffix. The function does not check whether the record currently exists.
    pub(crate) fn object_record_path(&self, object_hash: ObjectHash) -> PathBuf {
        self.object_records_dir()
            .join(format!("{}.json", object_hash.to_hex()))
    }

    /// Returns the canonical working-store path for an fs-file without
    /// checking that the path exists or contains the declared hash.
    pub(crate) fn fs_file_path_unchecked(&self, hash: FsFileHash) -> PathBuf {
        let hex = hash.to_hex();
        self.root().join(FS_FILES_DIR).join(&hex[..2]).join(hex)
    }
}

impl ReadOnlyStore {
    /// Opens and validates an existing store without changing it.
    ///
    /// `root` follows the same absolute-directory rules as [`Store::create`].
    /// Every standard store subdirectory must already exist as a real
    /// directory. Missing or malformed layout entries are reported rather than
    /// created, making a misspelled or incomplete secondary store fail closed.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let root = validate_root(root)?;
        validate_store_layout(&root)?;
        Ok(Self {
            inner: Arc::new(ReadOnlyStoreInner { root }),
        })
    }

    /// Returns the canonical store root.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub(crate) fn objects_dir(&self) -> PathBuf {
        self.root().join(OBJECTS_DIR)
    }

    pub(crate) fn builds_dir(&self) -> PathBuf {
        self.root().join(BUILDS_DIR)
    }

    pub(crate) fn reuses_dir(&self) -> PathBuf {
        self.root().join(REUSES_DIR)
    }

    pub(crate) fn object_path_unchecked(&self, object_hash: ObjectHash) -> PathBuf {
        self.objects_dir().join(object_hash.to_hex())
    }

    pub(crate) fn build_ref_path(&self, build_key: BuildKey) -> PathBuf {
        self.builds_dir().join(build_key.to_hex())
    }

    pub(crate) fn reuse_ref_path(&self, reuse_key: ReuseKey) -> PathBuf {
        self.reuses_dir().join(reuse_key.to_hex())
    }

    /// Returns the canonical read-only-store path for an fs-file without
    /// checking that the path exists or contains the declared hash.
    pub(crate) fn fs_file_path_unchecked(&self, hash: FsFileHash) -> PathBuf {
        let hex = hash.to_hex();
        self.root().join(FS_FILES_DIR).join(&hex[..2]).join(hex)
    }
}

fn validate_root(root: &Path) -> Result<PathBuf, StoreError> {
    if !root.is_absolute() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be absolute: '{}'",
            root.display()
        )));
    }
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StoreError::InvalidInput(format!("store root must exist: '{}'", root.display()))
        } else {
            StoreError::Io(format!(
                "failed to resolve store root '{}': {error}",
                root.display()
            ))
        }
    })?;
    let metadata = fs::metadata(&canonical_root).map_err(|error| {
        StoreError::Io(format!(
            "failed to inspect store root '{}': {error}",
            canonical_root.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be a directory: '{}'",
            root.display()
        )));
    }
    Ok(canonical_root)
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

fn ensure_store_layout(root: &Path) -> Result<(), StoreError> {
    ensure_store_dir(&root.join(OBJECTS_DIR), "objects")?;
    ensure_store_dir(&root.join(BUILDS_DIR), "builds")?;
    ensure_store_dir(&root.join(REUSES_DIR), "reuses")?;
    ensure_store_dir(&root.join(OBJECT_RECORDS_DIR), "object-records")?;
    ensure_store_dir(&root.join(OBJECT_REFS_DIR), "object-refs")?;
    ensure_store_dir(&root.join(FS_FILES_DIR), "fs-files")?;
    ensure_store_dir(&root.join(FS_TREES_DIR), "fs-trees")?;
    ensure_store_dir(&root.join(FS_TREE_REFS_DIR), "fs-tree-refs")?;
    Ok(())
}

fn validate_store_dir(path: &Path, label: &str) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StoreError::InvalidData(format!(
                "store {label} directory is missing: '{}'",
                path.display()
            ))
        } else {
            StoreError::Io(format!(
                "failed to inspect store {label} directory '{}': {error}",
                path.display()
            ))
        }
    })?;
    if !metadata.file_type().is_dir() {
        return Err(StoreError::InvalidData(format!(
            "store {label} path '{}' is not a directory",
            path.display()
        )));
    }
    Ok(())
}

fn validate_store_layout(root: &Path) -> Result<(), StoreError> {
    validate_store_dir(&root.join(OBJECTS_DIR), "objects")?;
    validate_store_dir(&root.join(BUILDS_DIR), "builds")?;
    validate_store_dir(&root.join(REUSES_DIR), "reuses")?;
    validate_store_dir(&root.join(OBJECT_RECORDS_DIR), "object-records")?;
    validate_store_dir(&root.join(OBJECT_REFS_DIR), "object-refs")?;
    validate_store_dir(&root.join(FS_FILES_DIR), "fs-files")?;
    validate_store_dir(&root.join(FS_TREES_DIR), "fs-trees")?;
    validate_store_dir(&root.join(FS_TREE_REFS_DIR), "fs-tree-refs")?;
    Ok(())
}
