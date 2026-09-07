use crate::fs_tree::{
    FsFileHash, FsTreeEntry, FsTreeManifest, normalize_fs_file_metadata, read_manifest_if_marked,
};
use crate::local_content::{
    RepositoryStagingGuard, allocate_repository_staging_path, create_repository_fs_file_staging,
    verify_fs_file,
};
use crate::object::import_object_with_expected_hash;
use crate::secondary::{ContentImportOutcome, ContentSource, ContentTransferMode, LocalRepository};
use crate::{ReadOnlyStore, Store, StoreError};
use bobr_core::ObjectHash;
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_runtime::runtime_provider::{RuntimeProvider, runtime_provider_for_current_process};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

/// Copy-oriented content import backed by a local read-only repository.
///
/// Objects and fs-files are copied into private working-store staging,
/// verified, and atomically published as independent inodes.
#[derive(Debug, Clone)]
pub struct LocalCopyContentSource {
    repository: LocalRepository,
    runtime: RuntimeProvider,
}

impl LocalCopyContentSource {
    /// Exposes a local repository's ordinary objects through copy import.
    pub fn new(repository: LocalRepository) -> Self {
        Self::with_runtime(repository, runtime_provider_for_current_process())
    }

    /// Wraps a repository with an explicitly selected runtime provider.
    ///
    /// Fs-file ownership is logical rootfs metadata, so unprivileged callers
    /// use a namespace provider while tests and root callers can select a host
    /// provider.
    pub fn with_runtime(repository: LocalRepository, runtime: RuntimeProvider) -> Self {
        Self {
            repository,
            runtime,
        }
    }

    /// Returns the shared local repository backend.
    pub fn repository(&self) -> &LocalRepository {
        &self.repository
    }

    fn ensure_fs_files(
        &self,
        working: &Store,
        manifest: &FsTreeManifest,
    ) -> Result<(), StoreError> {
        let mut seen = HashSet::new();
        let hashes = manifest
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } if seen.insert(*hash) => Some(*hash),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.import_fs_files(working, &hashes)
    }
}

impl ContentSource for LocalCopyContentSource {
    fn transfer_mode(&self) -> ContentTransferMode {
        ContentTransferMode::Copy
    }

    fn locate_objects(&self, hashes: &[ObjectHash]) -> Result<HashSet<ObjectHash>, StoreError> {
        self.repository.content().locate_objects(hashes)
    }

    fn object_manifest(&self, hash: ObjectHash) -> Result<Option<FsTreeManifest>, StoreError> {
        self.repository.content().object_manifest(hash)
    }

    fn locate_fs_files(&self, hashes: &[FsFileHash]) -> Result<HashSet<FsFileHash>, StoreError> {
        self.repository.content().locate_fs_files(hashes)
    }

    fn import_fs_files(&self, working: &Store, hashes: &[FsFileHash]) -> Result<(), StoreError> {
        if hashes.is_empty() {
            return Ok(());
        }
        let mut seen = HashSet::new();
        for hash in hashes {
            if !seen.insert(*hash) {
                continue;
            }
            let working_path = working.fs_file_path_unchecked(*hash);
            match fs::symlink_metadata(&working_path) {
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(map_io(&working_path, "inspect working fs-file", error));
                }
            }
            if self.repository.content().fs_file_path(*hash)?.is_none() {
                return Err(StoreError::InvalidData(format!(
                    "local repository fs-file '{hash}' is absent"
                )));
            }
        }
        self.runtime
            .run(
                &CopyFsFilesFunction,
                CopyFsFilesInput {
                    source_root: self.repository.store().root().to_path_buf(),
                    working_root: working.root().to_path_buf(),
                    hashes: hashes.iter().map(FsFileHash::to_hex).collect(),
                },
            )
            .map_err(|error| {
                StoreError::Io(format!(
                    "failed to copy fs-files from local repository '{}': {error}",
                    self.repository.store().root().display()
                ))
            })
    }

    fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentImportOutcome, StoreError> {
        if let Some(working_path) = working.object_path(hash)? {
            if let Some(manifest) = read_manifest_if_marked(&working_path)? {
                self.ensure_fs_files(working, &manifest)?;
            }
            return Ok(ContentImportOutcome::AlreadyPresent);
        }

        let Some(source_path) = self.repository.content().object_path(hash)? else {
            return Ok(ContentImportOutcome::NotFound);
        };
        if let Some(manifest) = self.repository.content().object_manifest(hash)? {
            self.ensure_fs_files(working, &manifest)?;
        }

        let staging_path = allocate_repository_staging_path(working)?;
        let mut guard = RepositoryStagingGuard::new(staging_path.clone());
        copy_ordinary_object(&source_path, &staging_path)?;
        import_object_with_expected_hash(working, &staging_path, hash)?;
        guard.disarm();
        Ok(ContentImportOutcome::Imported)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CopyFsFilesInput {
    source_root: PathBuf,
    working_root: PathBuf,
    hashes: Vec<String>,
}

/// Namespace operation that copies fs-files with their identity metadata.
#[derive(Debug)]
pub(crate) struct CopyFsFilesFunction;

impl RuntimeFunction for CopyFsFilesFunction {
    type Input = CopyFsFilesInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "repository-copy-fs-files"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        copy_fs_files(input).map_err(|error| RuntimeError::new(error.to_string()))
    }
}

fn copy_fs_files(input: CopyFsFilesInput) -> Result<(), StoreError> {
    let source = ReadOnlyStore::open(&input.source_root)?;
    let working = Store::create(&input.working_root)?;
    if source.root() == working.root() {
        return Err(StoreError::InvalidInput(format!(
            "local repository '{}' is the working store",
            source.root().display()
        )));
    }
    let repository = LocalRepository::new(source);
    let mut seen = HashSet::new();
    for raw_hash in input.hashes {
        let hash = raw_hash.parse::<FsFileHash>().map_err(|error| {
            StoreError::InvalidInput(format!("invalid fs-file hash '{raw_hash}': {error}"))
        })?;
        if !seen.insert(hash) {
            continue;
        }

        let destination = working.fs_file_path_unchecked(hash);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                verify_fs_file(&destination, hash)?;
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(map_io(&destination, "inspect working fs-file", error)),
        }

        let source_path = repository.content().fs_file_path(hash)?.ok_or_else(|| {
            StoreError::InvalidData(format!("local repository fs-file '{hash}' is absent"))
        })?;
        verify_fs_file(&source_path, hash)?;
        copy_one_fs_file(&source_path, &working, hash)?;
    }
    Ok(())
}

fn copy_one_fs_file(
    source: &Path,
    working: &Store,
    expected: FsFileHash,
) -> Result<(), StoreError> {
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|error| map_io(source, "inspect repository fs-file", error))?;
    let (staging_path, mut staging_file) = create_repository_fs_file_staging(working)?;
    let mut guard = RepositoryStagingGuard::new(staging_path.clone());
    let mut source_file =
        fs::File::open(source).map_err(|error| map_io(source, "open repository fs-file", error))?;
    io::copy(&mut source_file, &mut staging_file)
        .map_err(|error| map_copy_io(source, &staging_path, "copy fs-file", error))?;
    drop(staging_file);

    normalize_fs_file_metadata(
        &staging_path,
        source_metadata.uid(),
        source_metadata.gid(),
        source_metadata.permissions().mode() & 0o7777,
    )?;
    verify_fs_file(&staging_path, expected)?;

    let destination = working.fs_file_path_unchecked(expected);
    let parent = destination.parent().ok_or_else(|| {
        StoreError::InvalidData(format!(
            "working fs-file path has no parent: '{}'",
            destination.display()
        ))
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| map_io(parent, "create working fs-file shard", error))?;

    match rename_noreplace(&staging_path, &destination) {
        Ok(()) => {
            guard.disarm();
            if let Err(error) = verify_fs_file(&destination, expected) {
                let _ = fs::remove_file(&destination);
                return Err(error);
            }
        }
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            verify_fs_file(&destination, expected)?;
        }
        Err(error) => {
            return Err(map_copy_io(
                &staging_path,
                &destination,
                "publish copied fs-file",
                error,
            ));
        }
    }
    Ok(())
}

fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
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
    use std::sync::{Arc, Barrier};
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
        LocalCopyContentSource::with_runtime(
            LocalRepository::new(ReadOnlyStore::open(root).unwrap()),
            RuntimeProvider::host(),
        )
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
        assert!(
            fs::read_dir(store.root().join(crate::store::FS_FILES_DIR))
                .unwrap()
                .all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".bobr-repository-fs-file-import-")
                })
        );
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
    fn copies_fs_tree_manifest_and_metadata_complete_fs_files() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"payload\n").unwrap();
        fs::set_permissions(tree.join("payload"), fs::Permissions::from_mode(0o751)).unwrap();
        let manifest = repository.fs_tree().intern_tree(tree).unwrap();
        let file_hash = manifest
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .unwrap();
        let staged = temp.path().join("manifest");
        manifest.write_canonical(&staged).unwrap();
        let hash = publish(&repository, &staged, "manifest");
        let source_manifest = repository.object_path(hash).unwrap().unwrap();
        let source_file = repository.fs_file_path_unchecked(file_hash);
        let source_metadata = fs::metadata(&source_file).unwrap();
        let source = copy_source(repository.root());

        assert_eq!(
            source.import_object(&working, hash).unwrap(),
            ContentImportOutcome::Imported
        );
        let working_manifest = working.object_path(hash).unwrap().unwrap();
        let working_file = working.fs_file_path_unchecked(file_hash);
        assert_independent_inodes(&source_manifest, &working_manifest);
        assert_independent_inodes(&source_file, &working_file);
        let working_metadata = fs::metadata(&working_file).unwrap();
        assert_eq!(working_metadata.uid(), source_metadata.uid());
        assert_eq!(working_metadata.gid(), source_metadata.gid());
        assert_eq!(working_metadata.mode() & 0o7777, 0o751);
        assert_eq!(working_metadata.mtime(), bobr_core::CANONICAL_TIMESTAMP);
        assert_eq!(working_metadata.mtime_nsec(), 0);
        assert!(working.object_is_complete(hash).unwrap());

        fs::remove_file(source_manifest).unwrap();
        fs::remove_file(source_file).unwrap();
        assert_eq!(fs::read(working_file).unwrap(), b"payload\n");
        assert!(working.object_is_complete(hash).unwrap());
        assert_no_staging_entries(&working);
    }

    #[test]
    fn missing_fs_file_rejects_partial_closure_before_manifest_publication() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("a"), b"first\n").unwrap();
        fs::write(tree.join("b"), b"second\n").unwrap();
        let manifest = repository.fs_tree().intern_tree(tree).unwrap();
        let hashes = manifest
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .collect::<Vec<_>>();
        let staged = temp.path().join("manifest");
        manifest.write_canonical(&staged).unwrap();
        let object_hash = publish(&repository, &staged, "manifest");
        fs::remove_file(repository.fs_file_path_unchecked(hashes[1])).unwrap();

        let error = copy_source(repository.root())
            .import_object(&working, object_hash)
            .unwrap_err();
        assert!(error.to_string().contains("fs-file"), "{error}");
        assert!(error.to_string().contains("is absent"), "{error}");
        assert!(working.object_path(object_hash).unwrap().is_none());
        assert!(
            hashes
                .iter()
                .all(|hash| !working.fs_file_path_unchecked(*hash).exists())
        );
        assert_no_staging_entries(&working);
    }

    #[test]
    fn concurrent_fs_file_publishers_accept_the_same_verified_winner() {
        let temp = tempdir().unwrap();
        let repository = empty_store(&temp.path().join("repository"));
        let working = empty_store(&temp.path().join("working"));
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"concurrent copy\n").unwrap();
        let manifest = repository.fs_tree().intern_tree(tree).unwrap();
        let hash = manifest
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .unwrap();
        let source = repository.fs_file_path_unchecked(hash);
        let barrier = Arc::new(Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let source = source.clone();
                let working = working.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    copy_one_fs_file(&source, &working, hash)
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        verify_fs_file(&working.fs_file_path_unchecked(hash), hash).unwrap();
        assert_independent_inodes(&source, &working.fs_file_path_unchecked(hash));
        assert_no_staging_entries(&working);
    }
}
