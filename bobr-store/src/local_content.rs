use crate::StoreError;
use crate::fs_tree::{FsFileHash, FsTreeManifest, hash_fs_file_path, read_manifest_if_marked};
use crate::store::ReadOnlyStore;
use bobr_core::ObjectHash;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REPOSITORY_STAGING: AtomicU64 = AtomicU64::new(0);
static NEXT_REPOSITORY_FS_FILE_STAGING: AtomicU64 = AtomicU64::new(0);

/// Validated read-only access to content in a local store.
///
/// This helper owns content discovery and manifest parsing shared by local
/// transports. It only returns canonical CAS paths derived from typed hashes
/// and rejects symlinks and unsupported entry types before a transport uses
/// them.
#[derive(Debug, Clone)]
pub(crate) struct LocalStoreContentReader {
    store: ReadOnlyStore,
}

impl LocalStoreContentReader {
    /// Creates a reader for an already validated read-only store.
    pub(crate) fn new(store: ReadOnlyStore) -> Self {
        Self { store }
    }

    /// Returns the distinct requested object hashes with valid CAS entries.
    pub(crate) fn locate_objects(
        &self,
        hashes: &[ObjectHash],
    ) -> Result<HashSet<ObjectHash>, StoreError> {
        let mut available = HashSet::new();
        let mut seen = HashSet::new();
        for hash in hashes {
            if !seen.insert(*hash) {
                continue;
            }
            if self.object_path(*hash)?.is_some() {
                available.insert(*hash);
            }
        }
        Ok(available)
    }

    /// Reads a marked fs-tree manifest, or returns `None` for absent and plain
    /// objects.
    pub(crate) fn object_manifest(
        &self,
        hash: ObjectHash,
    ) -> Result<Option<FsTreeManifest>, StoreError> {
        let Some(path) = self.object_path(hash)? else {
            return Ok(None);
        };
        read_manifest_if_marked(&path)
    }

    /// Returns the distinct requested fs-file hashes with valid CAS entries.
    pub(crate) fn locate_fs_files(
        &self,
        hashes: &[FsFileHash],
    ) -> Result<HashSet<FsFileHash>, StoreError> {
        let mut available = HashSet::new();
        let mut seen = HashSet::new();
        for hash in hashes {
            if !seen.insert(*hash) {
                continue;
            }
            if self.fs_file_path(*hash)?.is_some() {
                available.insert(*hash);
            }
        }
        Ok(available)
    }

    /// Returns the canonical object CAS path after validating its entry type.
    pub(crate) fn object_path(&self, hash: ObjectHash) -> Result<Option<PathBuf>, StoreError> {
        let path = self.store.object_path_unchecked(hash);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_dir() => {
                Ok(Some(path))
            }
            Ok(_) => Err(StoreError::InvalidData(format!(
                "local repository object path '{}' is neither a regular file nor a directory",
                path.display()
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_io(&path, "inspect local repository object", error)),
        }
    }

    /// Returns the canonical fs-file CAS path after validating its entry type.
    pub(crate) fn fs_file_path(&self, hash: FsFileHash) -> Result<Option<PathBuf>, StoreError> {
        let path = self.store.fs_file_path_unchecked(hash);
        let shard = path.parent().ok_or_else(|| {
            StoreError::InvalidData(format!(
                "local repository fs-file path has no shard directory: '{}'",
                path.display()
            ))
        })?;
        match fs::symlink_metadata(shard) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(StoreError::InvalidData(format!(
                    "local repository fs-file shard path '{}' is not a directory",
                    shard.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(map_io(
                    shard,
                    "inspect local repository fs-file shard",
                    error,
                ));
            }
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => Ok(Some(path)),
            Ok(_) => Err(StoreError::InvalidData(format!(
                "local repository fs-file path '{}' is not a regular file",
                path.display()
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_io(&path, "inspect local repository fs-file", error)),
        }
    }
}

/// Allocates a private, nonexistent staging path on the working CAS filesystem.
pub(crate) fn allocate_repository_staging_path(
    working: &crate::Store,
) -> Result<PathBuf, StoreError> {
    loop {
        let serial = NEXT_REPOSITORY_STAGING.fetch_add(1, Ordering::Relaxed);
        let path = working.objects_dir().join(format!(
            ".bobr-repository-import-{}-{serial}",
            std::process::id()
        ));
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
            Ok(_) => {}
            Err(error) => {
                return Err(map_io(&path, "inspect repository staging path", error));
            }
        }
    }
}

/// Creates a private fs-file staging file on the working CAS filesystem.
pub(crate) fn create_repository_fs_file_staging(
    working: &crate::Store,
) -> Result<(PathBuf, fs::File), StoreError> {
    loop {
        let serial = NEXT_REPOSITORY_FS_FILE_STAGING.fetch_add(1, Ordering::Relaxed);
        let path = working
            .root()
            .join(crate::store::FS_FILES_DIR)
            .join(format!(
                ".bobr-repository-fs-file-import-{}-{serial}",
                std::process::id()
            ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(map_io(
                    &path,
                    "create repository fs-file staging file",
                    error,
                ));
            }
        }
    }
}

/// Verifies one canonical fs-file entry against its content identity.
pub(crate) fn verify_fs_file(path: &Path, expected: FsFileHash) -> Result<(), StoreError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| map_io(path, "inspect fs-file", error))?;
    if !metadata.file_type().is_file() {
        return Err(StoreError::InvalidData(format!(
            "fs-file path '{}' is not a regular file",
            path.display()
        )));
    }
    if metadata.mtime() != bobr_core::CANONICAL_TIMESTAMP || metadata.mtime_nsec() != 0 {
        return Err(StoreError::InvalidData(format!(
            "fs-file '{}' has noncanonical mtime {}.{:09}; expected {}.000000000",
            path.display(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            bobr_core::CANONICAL_TIMESTAMP
        )));
    }
    let actual = hash_fs_file_path(path)?;
    if actual != expected {
        return Err(StoreError::InvalidData(format!(
            "fs-file hash mismatch for '{}': expected '{expected}', got '{actual}'",
            path.display()
        )));
    }
    Ok(())
}

/// Removes an incomplete repository import unless it has been published.
pub(crate) struct RepositoryStagingGuard {
    path: PathBuf,
    armed: bool,
}

impl RepositoryStagingGuard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RepositoryStagingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = bobr_core::fsutil::remove_path_force(&self.path);
        }
    }
}

fn map_io(path: &Path, action: &str, error: io::Error) -> StoreError {
    StoreError::Io(format!("failed to {action} '{}': {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_tree::FsTreeEntry;
    use crate::{Store, import_build};
    use bobr_core::{BuildKey, ReuseKey};
    use std::os::unix::fs::symlink;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn object_hash(byte: char) -> ObjectHash {
        ObjectHash::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn build_key(byte: char) -> BuildKey {
        BuildKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn reuse_key(byte: char) -> ReuseKey {
        ReuseKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn store_and_reader() -> (tempfile::TempDir, Store, LocalStoreContentReader) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        fs::create_dir(&root).unwrap();
        let store = Store::create(&root).unwrap();
        let reader = LocalStoreContentReader::new(ReadOnlyStore::open(&root).unwrap());
        (temp, store, reader)
    }

    #[test]
    fn batched_lookup_deduplicates_present_objects_and_omits_absent_ones() {
        let (temp, store, reader) = store_and_reader();
        let staged = temp.path().join("object");
        fs::write(&staged, b"object\n").unwrap();
        let present = import_build(
            &store,
            build_key('1'),
            reuse_key('2'),
            Vec::new(),
            &staged,
            "object",
            "test-run",
        )
        .unwrap();
        let absent = object_hash('3');

        assert_eq!(
            reader.locate_objects(&[absent, present, present]).unwrap(),
            HashSet::from([present])
        );
        assert_eq!(reader.object_path(absent).unwrap(), None);
        assert_eq!(reader.object_manifest(absent).unwrap(), None);
    }

    #[test]
    fn batched_lookup_deduplicates_present_fs_files_and_omits_absent_ones() {
        let (temp, store, reader) = store_and_reader();
        let source = temp.path().join("tree");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("payload"), b"payload\n").unwrap();
        let manifest = store.fs_tree().intern_tree(source).unwrap();
        let present = manifest
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .unwrap();
        let absent = "f".repeat(64).parse::<FsFileHash>().unwrap();

        assert_eq!(
            reader.locate_fs_files(&[absent, present, present]).unwrap(),
            HashSet::from([present])
        );
        assert_eq!(reader.fs_file_path(absent).unwrap(), None);
    }

    #[test]
    fn object_lookup_and_manifest_reader_reject_symlinks() {
        let (_temp, store, reader) = store_and_reader();
        let symlink_hash = object_hash('4');
        symlink("missing", store.object_path_unchecked(symlink_hash)).unwrap();
        let error = reader.locate_objects(&[symlink_hash]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("neither a regular file nor a directory"),
            "{error}"
        );
        let error = reader.object_manifest(symlink_hash).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("neither a regular file nor a directory"),
            "{error}"
        );
    }

    #[test]
    fn fs_file_lookup_rejects_wrong_types_and_symlink_shards() {
        let (_temp, store, reader) = store_and_reader();
        let wrong_type = "6".repeat(64).parse::<FsFileHash>().unwrap();
        let wrong_type_path = store.fs_file_path_unchecked(wrong_type);
        fs::create_dir_all(wrong_type_path.parent().unwrap()).unwrap();
        fs::create_dir(&wrong_type_path).unwrap();
        let error = reader.locate_fs_files(&[wrong_type]).unwrap_err();
        assert!(
            error.to_string().contains("is not a regular file"),
            "{error}"
        );

        let escaped = "7".repeat(64).parse::<FsFileHash>().unwrap();
        let shard = store
            .fs_file_path_unchecked(escaped)
            .parent()
            .unwrap()
            .to_path_buf();
        symlink("../objects", &shard).unwrap();
        let error = reader.fs_file_path(escaped).unwrap_err();
        assert!(
            error.to_string().contains("shard path")
                && error.to_string().contains("is not a directory"),
            "{error}"
        );
    }

    #[test]
    fn marked_malformed_manifest_is_an_error() {
        let (_temp, store, reader) = store_and_reader();
        let hash = object_hash('8');
        fs::write(
            store.object_path_unchecked(hash),
            b"{\"schema\":\"bobr-fs-tree-manifest\"}\nnot-json\n",
        )
        .unwrap();

        let error = reader.object_manifest(hash).unwrap_err();
        assert!(matches!(error, StoreError::InvalidData(_)), "{error}");
        assert!(
            error.to_string().contains("failed to parse line 2"),
            "{error}"
        );
    }
}
