use crate::fs_tree::{FsFileHash, FsTreeManifest, read_manifest_if_marked};
use crate::local_content::{RepositoryStagingGuard, allocate_repository_staging_path};
use crate::object::import_object_with_expected_hash;
use crate::secondary::{ContentImportOutcome, ContentSource, LocalRepository};
use crate::{Store, StoreError};
use bobr_core::ObjectHash;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

/// Copy-oriented content import backed by a local read-only repository.
///
/// Ordinary file and directory objects are copied into private working-store
/// staging and verified before publication. Fs-tree closure transfer is kept
/// explicit and remains unsupported until its metadata-preserving copier is
/// available.
#[derive(Debug, Clone)]
pub struct LocalCopyContentSource {
    repository: LocalRepository,
}

impl LocalCopyContentSource {
    /// Exposes a local repository's ordinary objects through copy import.
    pub fn new(repository: LocalRepository) -> Self {
        Self { repository }
    }

    /// Returns the shared local repository backend.
    pub fn repository(&self) -> &LocalRepository {
        &self.repository
    }
}

impl ContentSource for LocalCopyContentSource {
    fn locate_objects(&self, hashes: &[ObjectHash]) -> Result<HashSet<ObjectHash>, StoreError> {
        self.repository.content().locate_objects(hashes)
    }

    fn object_manifest(&self, hash: ObjectHash) -> Result<Option<FsTreeManifest>, StoreError> {
        self.repository.content().object_manifest(hash)
    }

    fn locate_fs_files(&self, hashes: &[FsFileHash]) -> Result<HashSet<FsFileHash>, StoreError> {
        self.repository.content().locate_fs_files(hashes)
    }

    fn import_fs_files(&self, _working: &Store, hashes: &[FsFileHash]) -> Result<(), StoreError> {
        if hashes.is_empty() {
            return Ok(());
        }
        Err(StoreError::Unsupported(
            "local repository copy transport for fs-files is not implemented yet".to_string(),
        ))
    }

    fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentImportOutcome, StoreError> {
        if let Some(working_path) = working.object_path(hash)? {
            if read_manifest_if_marked(&working_path)?.is_some() {
                return Err(fs_tree_copy_unsupported(hash));
            }
            return Ok(ContentImportOutcome::AlreadyPresent);
        }

        let Some(source_path) = self.repository.content().object_path(hash)? else {
            return Ok(ContentImportOutcome::NotFound);
        };
        if self.repository.content().object_manifest(hash)?.is_some() {
            return Err(fs_tree_copy_unsupported(hash));
        }

        let staging_path = allocate_repository_staging_path(working)?;
        let mut guard = RepositoryStagingGuard::new(staging_path.clone());
        copy_ordinary_object(&source_path, &staging_path)?;
        import_object_with_expected_hash(working, &staging_path, hash)?;
        guard.disarm();
        Ok(ContentImportOutcome::Imported)
    }
}

fn fs_tree_copy_unsupported(hash: ObjectHash) -> StoreError {
    StoreError::Unsupported(format!(
        "local repository copy transport for fs-tree object '{hash}' is not implemented yet"
    ))
}

fn copy_ordinary_object(source: &Path, destination: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| map_io(source, "inspect repository object entry", error))?;
    let file_type = metadata.file_type();
    if file_type.is_file() {
        return copy_regular_file(source, destination, &metadata);
    }
    if !file_type.is_dir() {
        return Err(StoreError::Unsupported(format!(
            "repository object entry '{}' has an unsupported filesystem type",
            source.display()
        )));
    }

    let final_mode = object_mode(&metadata);
    fs::create_dir(destination)
        .map_err(|error| map_io(destination, "create copied object directory", error))?;
    fs::set_permissions(destination, fs::Permissions::from_mode(final_mode | 0o700))
        .map_err(|error| map_io(destination, "prepare copied object directory", error))?;

    let mut children = fs::read_dir(source)
        .map_err(|error| map_io(source, "read repository object directory", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_io(source, "read repository object directory entry", error))?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        let source_path = child.path();
        let destination_path = destination.join(child.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| map_io(&source_path, "inspect repository object entry", error))?;
        let file_type = metadata.file_type();
        if file_type.is_file() {
            copy_regular_file(&source_path, &destination_path, &metadata)?;
        } else if file_type.is_dir() {
            copy_ordinary_object(&source_path, &destination_path)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&source_path)
                .map_err(|error| map_io(&source_path, "read repository object symlink", error))?;
            symlink(&target, &destination_path)
                .map_err(|error| map_io(&destination_path, "recreate object symlink", error))?;
        } else {
            return Err(StoreError::Unsupported(format!(
                "repository object entry '{}' has an unsupported filesystem type",
                source_path.display()
            )));
        }
    }

    fs::set_permissions(destination, fs::Permissions::from_mode(final_mode))
        .map_err(|error| map_io(destination, "finalize copied object directory", error))
}

fn copy_regular_file(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
) -> Result<(), StoreError> {
    fs::copy(source, destination)
        .map_err(|error| map_copy_io(source, destination, "copy object file", error))?;
    fs::set_permissions(
        destination,
        fs::Permissions::from_mode(object_mode(metadata)),
    )
    .map_err(|error| map_io(destination, "set copied object file mode", error))
}

fn object_mode(metadata: &fs::Metadata) -> u32 {
    metadata.permissions().mode() & 0o7777
}

fn map_io(path: &Path, action: &str, error: io::Error) -> StoreError {
    StoreError::Io(format!("failed to {action} '{}': {error}", path.display()))
}

fn map_copy_io(source: &Path, destination: &Path, action: &str, error: io::Error) -> StoreError {
    StoreError::Io(format!(
        "failed to {action} '{}' -> '{}': {error}",
        source.display(),
        destination.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalRepository, ReadOnlyStore, import_build};
    use bobr_core::{BuildKey, ReuseKey};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn build_key(byte: char) -> BuildKey {
        BuildKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn reuse_key(byte: char) -> ReuseKey {
        ReuseKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn object_hash(byte: char) -> ObjectHash {
        ObjectHash::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn empty_store(root: &Path) -> Store {
        fs::create_dir(root).unwrap();
        Store::create(root).unwrap()
    }

    fn copy_source(root: &Path) -> LocalCopyContentSource {
        LocalCopyContentSource::new(LocalRepository::new(ReadOnlyStore::open(root).unwrap()))
    }

    fn publish(store: &Store, staged: &Path, name: &str) -> ObjectHash {
        import_build(
            store,
            build_key('1'),
            reuse_key('2'),
            Vec::new(),
            staged,
            name,
            "test-run",
        )
        .unwrap()
    }

    fn assert_independent_inodes(source: &Path, destination: &Path) {
        let source = fs::metadata(source).unwrap();
        let destination = fs::metadata(destination).unwrap();
        assert_eq!(source.dev(), destination.dev());
        assert_ne!(source.ino(), destination.ino());
    }

    fn assert_no_staging_entries(store: &Store) {
        assert!(fs::read_dir(store.objects_dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".bobr-repository-import-")
        }));
    }

    #[test]
    fn copies_regular_object_to_an_independent_inode_and_survives_source_unlink() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let staged = temp.path().join("file");
        fs::write(&staged, b"copied object\n").unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o751)).unwrap();
        let hash = publish(&repository, &staged, "file");
        let source_path = repository.object_path(hash).unwrap().unwrap();
        let source = copy_source(repository.root());

        assert_eq!(
            source.import_object(&working, hash).unwrap(),
            ContentImportOutcome::Imported
        );
        assert_eq!(
            source.import_object(&working, hash).unwrap(),
            ContentImportOutcome::AlreadyPresent
        );
        let destination = working.object_path(hash).unwrap().unwrap();
        assert_independent_inodes(&source_path, &destination);
        assert_eq!(
            fs::metadata(&source_path).unwrap().permissions().mode() & 0o7777,
            fs::metadata(&destination).unwrap().permissions().mode() & 0o7777
        );

        fs::remove_file(source_path).unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"copied object\n");
    }

    #[test]
    fn recursively_copies_directories_without_following_symlinks() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let staged = temp.path().join("directory");
        fs::create_dir_all(staged.join("bin")).unwrap();
        fs::write(staged.join("bin/tool"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(staged.join("bin/tool"), fs::Permissions::from_mode(0o751)).unwrap();
        fs::create_dir(staged.join("empty")).unwrap();
        symlink("../outside", staged.join("link")).unwrap();
        let hash = publish(&repository, &staged, "directory");
        let source_path = repository.object_path(hash).unwrap().unwrap();
        let source = copy_source(repository.root());

        assert_eq!(
            source.import_object(&working, hash).unwrap(),
            ContentImportOutcome::Imported
        );
        let destination = working.object_path(hash).unwrap().unwrap();
        assert_independent_inodes(&source_path.join("bin/tool"), &destination.join("bin/tool"));
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("../outside")
        );
        assert!(destination.join("empty").is_dir());
        assert_eq!(fsobj_hash::hash_path(&destination).unwrap(), hash);

        bobr_core::fsutil::remove_path_force(&source_path).unwrap();
        assert_eq!(
            fs::read(destination.join("bin/tool")).unwrap(),
            b"#!/bin/sh\n"
        );
    }

    #[test]
    fn absent_object_is_a_content_miss_without_staging() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let missing = object_hash('5');

        assert_eq!(
            copy_source(repository.root())
                .import_object(&working, missing)
                .unwrap(),
            ContentImportOutcome::NotFound
        );
        assert!(working.object_path(missing).unwrap().is_none());
        assert_no_staging_entries(&working);
    }

    #[test]
    fn rejects_special_entries_and_removes_partial_staging() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let hash = object_hash('3');
        let object = repository.object_path_unchecked(hash);
        fs::create_dir(&object).unwrap();
        fs::write(object.join("before"), b"partial\n").unwrap();
        let fifo = object.join("fifo");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

        let error = copy_source(repository.root())
            .import_object(&working, hash)
            .unwrap_err();
        assert!(error.to_string().contains("unsupported filesystem type"));
        assert!(working.object_path(hash).unwrap().is_none());
        assert_no_staging_entries(&working);
    }

    #[test]
    fn expected_hash_mismatch_is_not_published_and_cleans_staging() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let staged = temp.path().join("actual");
        fs::write(&staged, b"actual object\n").unwrap();
        let actual = publish(&repository, &staged, "actual");
        let expected = object_hash('4');
        fs::hard_link(
            repository.object_path(actual).unwrap().unwrap(),
            repository.object_path_unchecked(expected),
        )
        .unwrap();

        let error = copy_source(repository.root())
            .import_object(&working, expected)
            .unwrap_err();
        assert!(
            error.to_string().contains("object hash mismatch"),
            "{error}"
        );
        assert!(working.object_path(expected).unwrap().is_none());
        assert_no_staging_entries(&working);
    }

    #[test]
    fn fs_tree_copy_is_explicitly_deferred() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"payload\n").unwrap();
        let manifest = repository.fs_tree().intern_tree(tree).unwrap();
        let staged = temp.path().join("manifest");
        manifest.write_canonical(&staged).unwrap();
        let hash = publish(&repository, &staged, "manifest");

        let error = copy_source(repository.root())
            .import_object(&working, hash)
            .unwrap_err();
        assert!(error.to_string().contains("fs-tree object"), "{error}");
        assert!(working.object_path(hash).unwrap().is_none());
    }
}
