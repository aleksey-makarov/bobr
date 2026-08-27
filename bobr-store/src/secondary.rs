//! Narrow read-only capabilities exposed by secondary stores.
//!
//! A trusted key index makes unverifiable build/reuse assertions. A content
//! source only reports self-verifiable content addressed by an already-known
//! object hash. Keeping the traits independent permits a small trusted index
//! to name content served by another, untrusted backend.

use crate::fs_tree::{
    FsFileHash, FsTreeEntry, FsTreeManifest, hash_fs_file_path, read_manifest_if_marked,
};
use crate::object::import_object_with_expected_hash;
use crate::refs::parse_object_target;
use crate::{ReadOnlyStore, Store, StoreError};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_runtime::runtime_provider::{RuntimeProvider, runtime_provider_for_current_process};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SECONDARY_STAGING: AtomicU64 = AtomicU64::new(0);

/// Trusted metadata resolving one build or reuse key to an object hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedResolution<K> {
    /// Build or reuse key that was resolved.
    pub key: K,
    /// Object named by the trusted mapping.
    pub object_hash: ObjectHash,
}

/// Read-only capability for trusted `BuildKey`/`ReuseKey` mappings.
///
/// Implementations must validate their index representation, but deliberately
/// do not require the corresponding object content to be present. Content
/// availability belongs to [`ContentSource`], and an index may be replicated
/// independently from the bytes it names.
pub trait TrustedKeyIndex: fmt::Debug + Send + Sync {
    /// Resolves every available build key in one batch.
    ///
    /// Missing keys are omitted from the returned results. Malformed mappings
    /// fail the whole batch rather than being silently treated as misses.
    /// Object records are not opened.
    fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<TrustedResolution<BuildKey>>, StoreError>;

    /// Resolves every available reuse key in one batch.
    ///
    /// Missing keys are omitted from the returned results. Malformed mappings
    /// fail the whole batch rather than being silently treated as misses.
    /// Object records are not opened.
    fn resolve_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<TrustedResolution<ReuseKey>>, StoreError>;
}

/// Read-only capability for content addressed by a known object hash.
///
/// Implementations perform batched availability discovery and verified import
/// into a working store without gaining access to trusted key mappings.
pub trait ContentSource: fmt::Debug + Send + Sync {
    /// Returns the subset of `hashes` whose top-level object payload exists.
    ///
    /// This does not claim that an fs-tree's referenced fs-files are complete;
    /// the resolver discovers that closure through [`Self::object_manifest`]
    /// and [`Self::locate_fs_files`].
    fn locate_objects(&self, hashes: &[ObjectHash]) -> Result<HashSet<ObjectHash>, StoreError>;

    /// Parses the object as an fs-tree manifest when it carries the canonical
    /// manifest schema marker.
    ///
    /// `None` means the object is absent or is an ordinary file/directory
    /// object. A marked but malformed manifest is an error.
    fn object_manifest(&self, hash: ObjectHash) -> Result<Option<FsTreeManifest>, StoreError>;

    /// Returns the subset of fs-file hashes present as regular files.
    fn locate_fs_files(&self, hashes: &[FsFileHash]) -> Result<HashSet<FsFileHash>, StoreError>;

    /// Imports a batch of fs-files into `working` using this source's transport.
    ///
    /// Every requested fs-file must exist and pass metadata, timestamp, and
    /// hash validation. The operation is idempotent for fs-files already in the
    /// working store.
    fn import_fs_files(&self, working: &Store, hashes: &[FsFileHash]) -> Result<(), StoreError>;

    /// Ensures one object and, for an fs-tree manifest, its fs-file closure in
    /// `working` using this source's transport.
    ///
    /// Content is verified against `hash` before the top-level object becomes
    /// visible. Missing content is reported separately from malformed or
    /// mismatched content.
    fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentImportOutcome, StoreError>;
}

/// Result of trying to import one known object from a content source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentImportOutcome {
    /// The source has no top-level object for this hash.
    NotFound,
    /// The working store already contained the complete object.
    AlreadyPresent,
    /// The object was imported and published in the working store.
    Imported,
}

/// Trusted build/reuse index backed by a local read-only store.
#[derive(Debug, Clone)]
pub struct LocalTrustedKeyIndex {
    store: ReadOnlyStore,
}

impl LocalTrustedKeyIndex {
    /// Wraps a validated read-only local store as a trusted key index.
    pub fn new(store: ReadOnlyStore) -> Self {
        Self { store }
    }

    /// Returns the underlying read-only store.
    pub fn store(&self) -> &ReadOnlyStore {
        &self.store
    }
}

impl TrustedKeyIndex for LocalTrustedKeyIndex {
    fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<TrustedResolution<BuildKey>>, StoreError> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            if seen.insert(*key)
                && let Some(object_hash) =
                    load_resolution("build", &self.store.build_ref_path(*key))?
            {
                found.push(TrustedResolution {
                    key: *key,
                    object_hash,
                });
            }
        }
        Ok(found)
    }

    fn resolve_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<TrustedResolution<ReuseKey>>, StoreError> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            if seen.insert(*key)
                && let Some(object_hash) =
                    load_resolution("reuse", &self.store.reuse_ref_path(*key))?
            {
                found.push(TrustedResolution {
                    key: *key,
                    object_hash,
                });
            }
        }
        Ok(found)
    }
}

/// Hardlink-oriented content availability backed by a local read-only store.
///
/// This type does not expose key-index operations. Import requires hardlinks
/// into a same-filesystem working store and never silently falls back to
/// copying.
#[derive(Debug, Clone)]
pub struct LocalHardlinkContentSource {
    store: ReadOnlyStore,
    runtime: RuntimeProvider,
}

impl LocalHardlinkContentSource {
    /// Wraps a validated read-only local store as a hardlink content source.
    pub fn new(store: ReadOnlyStore) -> Self {
        Self::with_runtime(store, runtime_provider_for_current_process())
    }

    /// Wraps a store with an explicitly selected runtime provider.
    ///
    /// Production callers normally use [`Self::new`]. Tests and root callers
    /// can select a host provider; unprivileged imports use a namespace provider
    /// so fs-files owned by mapped subordinate IDs can be hardlinked.
    pub fn with_runtime(store: ReadOnlyStore, runtime: RuntimeProvider) -> Self {
        Self { store, runtime }
    }

    /// Returns the underlying read-only store.
    pub fn store(&self) -> &ReadOnlyStore {
        &self.store
    }

    fn validate_working_store(&self, working: &Store) -> Result<(), StoreError> {
        if self.store.root() == working.root() {
            return Err(StoreError::InvalidInput(format!(
                "secondary store '{}' is the working store",
                self.store.root().display()
            )));
        }
        require_same_filesystem("objects", &self.store.objects_dir(), &working.objects_dir())
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

impl ContentSource for LocalHardlinkContentSource {
    fn locate_objects(&self, hashes: &[ObjectHash]) -> Result<HashSet<ObjectHash>, StoreError> {
        let mut available = HashSet::new();
        for hash in hashes {
            let path = self.store.object_path_unchecked(*hash);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_dir() => {
                    available.insert(*hash);
                }
                Ok(_) => {
                    return Err(StoreError::InvalidData(format!(
                        "secondary object path '{}' is neither a regular file nor a directory",
                        path.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(StoreError::Io(format!(
                        "failed to inspect secondary object '{}': {error}",
                        path.display()
                    )));
                }
            }
        }
        Ok(available)
    }

    fn object_manifest(&self, hash: ObjectHash) -> Result<Option<FsTreeManifest>, StoreError> {
        let path = self.store.object_path_unchecked(hash);
        match fs::symlink_metadata(&path) {
            Ok(_) => read_manifest_if_marked(&path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_io(&path, "inspect secondary object manifest", error)),
        }
    }

    fn locate_fs_files(&self, hashes: &[FsFileHash]) -> Result<HashSet<FsFileHash>, StoreError> {
        let mut available = HashSet::new();
        for hash in hashes {
            let path = self.store.fs_file_path_unchecked(*hash);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    available.insert(*hash);
                }
                Ok(_) => {
                    return Err(StoreError::InvalidData(format!(
                        "secondary fs-file path '{}' is not a regular file",
                        path.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(map_io(&path, "inspect secondary fs-file", error));
                }
            }
        }
        Ok(available)
    }

    fn import_fs_files(&self, working: &Store, hashes: &[FsFileHash]) -> Result<(), StoreError> {
        if hashes.is_empty() {
            return Ok(());
        }
        self.validate_working_store(working)?;
        self.runtime
            .run(
                &HardlinkFsFilesFunction,
                HardlinkFsFilesInput {
                    source_root: self.store.root().to_path_buf(),
                    working_root: working.root().to_path_buf(),
                    hashes: hashes.iter().map(FsFileHash::to_hex).collect(),
                },
            )
            .map_err(|error| {
                StoreError::Io(format!(
                    "failed to import fs-files from secondary store '{}': {error}",
                    self.store.root().display()
                ))
            })
    }

    fn import_object(
        &self,
        working: &Store,
        hash: ObjectHash,
    ) -> Result<ContentImportOutcome, StoreError> {
        self.validate_working_store(working)?;

        if let Some(working_path) = working.object_path(hash)? {
            if let Some(manifest) = read_manifest_if_marked(&working_path)? {
                self.ensure_fs_files(working, &manifest)?;
            }
            return Ok(ContentImportOutcome::AlreadyPresent);
        }

        let source_path = self.store.object_path_unchecked(hash);
        let source_metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ContentImportOutcome::NotFound);
            }
            Err(error) => {
                return Err(map_io(&source_path, "inspect secondary object", error));
            }
        };
        if !(source_metadata.file_type().is_file() || source_metadata.file_type().is_dir()) {
            return Err(StoreError::InvalidData(format!(
                "secondary object path '{}' is neither a regular file nor a directory",
                source_path.display()
            )));
        }

        if let Some(manifest) = read_manifest_if_marked(&source_path)? {
            self.ensure_fs_files(working, &manifest)?;
        }

        let staging_path = allocate_staging_path(working)?;
        let mut guard = StagingGuard::new(staging_path.clone());
        hardlink_object(&source_path, &staging_path)?;
        import_object_with_expected_hash(working, &staging_path, hash)?;
        guard.disarm();
        Ok(ContentImportOutcome::Imported)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HardlinkFsFilesInput {
    source_root: PathBuf,
    working_root: PathBuf,
    hashes: Vec<String>,
}

/// Namespace operation that hardlinks fs-files while their logical ownership
/// is visible through the current user-namespace mapping.
#[derive(Debug)]
pub(crate) struct HardlinkFsFilesFunction;

impl RuntimeFunction for HardlinkFsFilesFunction {
    type Input = HardlinkFsFilesInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "secondary-hardlink-fs-files"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        hardlink_fs_files(input).map_err(|error| RuntimeError::new(error.to_string()))
    }
}

fn hardlink_fs_files(input: HardlinkFsFilesInput) -> Result<(), StoreError> {
    let source = ReadOnlyStore::open(&input.source_root)?;
    let working = Store::create(&input.working_root)?;
    if source.root() == working.root() {
        return Err(StoreError::InvalidInput(format!(
            "secondary store '{}' is the working store",
            source.root().display()
        )));
    }
    require_same_filesystem(
        "fs-files",
        &source.root().join(crate::store::FS_FILES_DIR),
        &working.root().join(crate::store::FS_FILES_DIR),
    )?;

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
            Err(error) => {
                return Err(map_io(&destination, "inspect working fs-file", error));
            }
        }

        let source_path = source.fs_file_path_unchecked(hash);
        verify_fs_file(&source_path, hash)?;
        let parent = destination.parent().ok_or_else(|| {
            StoreError::InvalidData(format!(
                "working fs-file path has no parent: '{}'",
                destination.display()
            ))
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| map_io(parent, "create working fs-file shard", error))?;

        let created = match fs::hard_link(&source_path, &destination) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => {
                return Err(hardlink_error(&source_path, &destination, error));
            }
        };
        if let Err(error) = verify_fs_file(&destination, hash) {
            if created {
                let _ = fs::remove_file(&destination);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn verify_fs_file(path: &Path, expected: FsFileHash) -> Result<(), StoreError> {
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

fn require_same_filesystem(
    label: &str,
    source: &Path,
    destination: &Path,
) -> Result<(), StoreError> {
    let source_metadata = fs::metadata(source).map_err(|error| {
        map_io(
            source,
            &format!("inspect secondary {label} directory"),
            error,
        )
    })?;
    let destination_metadata = fs::metadata(destination).map_err(|error| {
        map_io(
            destination,
            &format!("inspect working {label} directory"),
            error,
        )
    })?;
    if source_metadata.dev() != destination_metadata.dev() {
        return Err(StoreError::Unsupported(format!(
            "secondary {label} directory '{}' and working {label} directory '{}' are on different filesystems; hardlink-only import requires one filesystem",
            source.display(),
            destination.display()
        )));
    }
    Ok(())
}

fn allocate_staging_path(working: &Store) -> Result<PathBuf, StoreError> {
    loop {
        let serial = NEXT_SECONDARY_STAGING.fetch_add(1, Ordering::Relaxed);
        let path = working.objects_dir().join(format!(
            ".bobr-secondary-import-{}-{serial}",
            std::process::id()
        ));
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
            Ok(_) => {}
            Err(error) => {
                return Err(map_io(&path, "inspect secondary staging path", error));
            }
        }
    }
}

struct StagingGuard {
    path: PathBuf,
    armed: bool,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = bobr_core::fsutil::remove_path_force(&self.path);
        }
    }
}

fn hardlink_object(source: &Path, destination: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| map_io(source, "inspect secondary object entry", error))?;
    let file_type = metadata.file_type();
    if file_type.is_file() {
        return fs::hard_link(source, destination)
            .map_err(|error| hardlink_error(source, destination, error));
    }
    if file_type.is_symlink() {
        let target = fs::read_link(source)
            .map_err(|error| map_io(source, "read secondary object symlink", error))?;
        return symlink(&target, destination)
            .map_err(|error| map_io(destination, "recreate secondary object symlink", error));
    }
    if !file_type.is_dir() {
        return Err(StoreError::Unsupported(format!(
            "secondary object entry '{}' has an unsupported filesystem type",
            source.display()
        )));
    }

    let final_mode = metadata.permissions().mode() & 0o7777;
    fs::create_dir(destination)
        .map_err(|error| map_io(destination, "create secondary object directory", error))?;
    fs::set_permissions(destination, fs::Permissions::from_mode(final_mode | 0o700))
        .map_err(|error| map_io(destination, "prepare secondary object directory", error))?;
    let mut children = fs::read_dir(source)
        .map_err(|error| map_io(source, "read secondary object directory", error))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_io(source, "read secondary object directory entry", error))?;
    children.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    for child in children {
        let name = child.file_name().ok_or_else(|| {
            StoreError::InvalidData(format!(
                "secondary object entry has no file name: '{}'",
                child.display()
            ))
        })?;
        hardlink_object(&child, &destination.join(name))?;
    }
    fs::set_permissions(destination, fs::Permissions::from_mode(final_mode))
        .map_err(|error| map_io(destination, "finalize secondary object directory", error))?;
    Ok(())
}

fn hardlink_error(source: &Path, destination: &Path, error: io::Error) -> StoreError {
    StoreError::Io(format!(
        "failed to hardlink secondary content '{}' -> '{}': {error}; hardlink-only import requires one filesystem and compatible ownership",
        source.display(),
        destination.display()
    ))
}

fn map_io(path: &Path, action: &str, error: io::Error) -> StoreError {
    StoreError::Io(format!("failed to {action} '{}': {error}", path.display()))
}

fn load_resolution(kind: &str, ref_path: &Path) -> Result<Option<ObjectHash>, StoreError> {
    let metadata = match fs::symlink_metadata(ref_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(StoreError::Io(format!(
                "failed to inspect {kind} ref '{}': {error}",
                ref_path.display()
            )));
        }
    };
    if !metadata.file_type().is_symlink() {
        return Err(StoreError::InvalidData(format!(
            "{kind} ref '{}' is not a symlink",
            ref_path.display()
        )));
    }

    let target = fs::read_link(ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read {kind} ref '{}': {error}",
            ref_path.display()
        ))
    })?;
    let object_hash = parse_object_target(kind, ref_path, &target)?;
    Ok(Some(object_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, import_build};
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use std::fs::{FileTimes, OpenOptions};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::str::FromStr;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::tempdir;

    fn build_key(byte: char) -> BuildKey {
        BuildKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn reuse_key(byte: char) -> ReuseKey {
        ReuseKey::from_str(&byte.to_string().repeat(64)).unwrap()
    }

    fn populated_store(root: &Path) -> (Store, BuildKey, ReuseKey, ObjectHash) {
        fs::create_dir(root).unwrap();
        let store = Store::create(root).unwrap();
        let build = build_key('1');
        let reuse = reuse_key('2');
        let staged = root.parent().unwrap().join("staged-object");
        fs::write(&staged, b"secondary object\n").unwrap();
        let object_hash = import_build(
            &store,
            build,
            reuse,
            Vec::new(),
            &staged,
            "secondary-object",
            "test-run",
        )
        .unwrap();
        (store, build, reuse, object_hash)
    }

    fn empty_store(root: &Path) -> Store {
        fs::create_dir(root).unwrap();
        Store::create(root).unwrap()
    }

    fn host_content_source(root: &Path) -> LocalHardlinkContentSource {
        LocalHardlinkContentSource::with_runtime(
            ReadOnlyStore::open(root).unwrap(),
            RuntimeProvider::host(),
        )
    }

    fn fs_tree_object(root: &Path) -> (Store, ObjectHash, FsFileHash) {
        let store = empty_store(root);
        let source = root.parent().unwrap().join("fs-tree-source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("payload"), b"fs-tree payload\n").unwrap();
        let manifest = store.fs_tree().intern_tree(source).unwrap();
        let file_hash = manifest
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .unwrap();
        let staged = root.parent().unwrap().join("fs-tree-manifest");
        manifest.write_canonical(&staged).unwrap();
        let object_hash = import_build(
            &store,
            build_key('6'),
            reuse_key('7'),
            Vec::new(),
            &staged,
            "fs-tree-object",
            "test-run",
        )
        .unwrap();
        (store, object_hash, file_hash)
    }

    #[test]
    fn read_only_open_requires_a_complete_existing_layout_without_creating_it() {
        let temp = tempdir().unwrap();
        let incomplete = temp.path().join("incomplete");
        fs::create_dir(&incomplete).unwrap();

        let error = ReadOnlyStore::open(&incomplete).unwrap_err();
        assert!(error.to_string().contains("objects directory is missing"));
        assert!(!incomplete.join("objects").exists());

        let complete = temp.path().join("complete");
        fs::create_dir(&complete).unwrap();
        Store::create(&complete).unwrap();
        let opened = ReadOnlyStore::open(&complete).unwrap();
        assert_eq!(opened.root(), fs::canonicalize(complete).unwrap());
    }

    #[test]
    fn trusted_index_resolves_build_and_reuse_batches_and_omits_misses() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        let (_store, build, reuse, object_hash) = populated_store(&root);
        let index = LocalTrustedKeyIndex::new(ReadOnlyStore::open(&root).unwrap());

        let builds = index
            .resolve_builds(&[build_key('3'), build, build])
            .unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].key, build);
        assert_eq!(builds[0].object_hash, object_hash);

        let reuses = index.resolve_reuses(&[reuse, reuse_key('4')]).unwrap();
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].key, reuse);
        assert_eq!(reuses[0].object_hash, object_hash);
    }

    #[test]
    fn trusted_index_resolution_does_not_require_object_content() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        let (store, build, reuse, object_hash) = populated_store(&root);
        fs::remove_file(store.object_path(object_hash).unwrap().unwrap()).unwrap();
        let read_only = ReadOnlyStore::open(&root).unwrap();
        let index = LocalTrustedKeyIndex::new(read_only.clone());
        let content = LocalHardlinkContentSource::new(read_only);

        assert_eq!(
            index.resolve_builds(&[build]).unwrap()[0].object_hash,
            object_hash
        );
        assert_eq!(
            index.resolve_reuses(&[reuse]).unwrap()[0].object_hash,
            object_hash
        );
        assert!(content.locate_objects(&[object_hash]).unwrap().is_empty());
    }

    #[test]
    fn index_validates_mapping_target_but_does_not_read_object_record() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        let (store, build, _reuse, object_hash) = populated_store(&root);
        let ref_path = store.build_ref_path(build);
        fs::remove_file(&ref_path).unwrap();
        symlink("../objects/not-a-record", &ref_path).unwrap();
        let index = LocalTrustedKeyIndex::new(ReadOnlyStore::open(&root).unwrap());

        let error = index.resolve_builds(&[build]).unwrap_err();
        assert!(error.to_string().contains("invalid object hash"));

        fs::remove_file(&ref_path).unwrap();
        let canonical_target = Path::new("../objects").join(object_hash.to_hex());
        symlink(canonical_target, &ref_path).unwrap();
        fs::remove_file(store.object_record_path(object_hash)).unwrap();
        let resolved = index.resolve_builds(&[build]).unwrap();
        assert_eq!(resolved[0].object_hash, object_hash);
        fs::write(store.object_record_path(object_hash), b"not json\n").unwrap();
        let resolved = index.resolve_builds(&[build]).unwrap();
        assert_eq!(resolved[0].object_hash, object_hash);
    }

    #[test]
    fn content_source_reports_only_real_file_or_directory_objects() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        let (store, _build, _reuse, object_hash) = populated_store(&root);
        let missing = ObjectHash::from_str(&"5".repeat(64)).unwrap();
        let source = LocalHardlinkContentSource::new(ReadOnlyStore::open(&root).unwrap());

        assert_eq!(
            source.locate_objects(&[missing, object_hash]).unwrap(),
            HashSet::from([object_hash])
        );

        fs::remove_file(store.object_path(object_hash).unwrap().unwrap()).unwrap();
        symlink("elsewhere", store.object_path_unchecked(object_hash)).unwrap();
        let error = source.locate_objects(&[object_hash]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("neither a regular file nor a directory")
        );
    }

    #[test]
    fn imports_regular_object_by_hardlink_and_survives_secondary_unlink() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let (secondary, _build, _reuse, object_hash) = populated_store(&secondary_root);
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);

        assert_eq!(
            source.import_object(&working, object_hash).unwrap(),
            ContentImportOutcome::Imported
        );
        let secondary_path = secondary.object_path(object_hash).unwrap().unwrap();
        let working_path = working.object_path(object_hash).unwrap().unwrap();
        assert_eq!(
            fs::metadata(&secondary_path).unwrap().ino(),
            fs::metadata(&working_path).unwrap().ino()
        );
        assert_eq!(
            source.import_object(&working, object_hash).unwrap(),
            ContentImportOutcome::AlreadyPresent
        );
        assert!(!working.object_record_path(object_hash).exists());

        fs::remove_file(secondary_path).unwrap();
        assert_eq!(fs::read(working_path).unwrap(), b"secondary object\n");
    }

    #[test]
    fn missing_object_is_a_content_miss_without_working_store_changes() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        empty_store(&secondary_root);
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);
        let missing = ObjectHash::from_str(&"b".repeat(64)).unwrap();

        assert_eq!(
            source.import_object(&working, missing).unwrap(),
            ContentImportOutcome::NotFound
        );
        assert!(working.object_path(missing).unwrap().is_none());
    }

    #[test]
    fn imports_directory_structure_while_hardlinking_regular_files() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let secondary = empty_store(&secondary_root);
        let staged = temp.path().join("directory-object");
        fs::create_dir_all(staged.join("bin")).unwrap();
        fs::write(staged.join("bin/tool"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(staged.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(staged.join("empty")).unwrap();
        symlink("bin/tool", staged.join("tool")).unwrap();
        let object_hash = import_build(
            &secondary,
            build_key('8'),
            reuse_key('9'),
            Vec::new(),
            &staged,
            "directory-object",
            "test-run",
        )
        .unwrap();
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);

        assert_eq!(
            source.import_object(&working, object_hash).unwrap(),
            ContentImportOutcome::Imported
        );
        let secondary_object = secondary.object_path(object_hash).unwrap().unwrap();
        let working_object = working.object_path(object_hash).unwrap().unwrap();
        assert_eq!(
            fs::metadata(secondary_object.join("bin/tool"))
                .unwrap()
                .ino(),
            fs::metadata(working_object.join("bin/tool")).unwrap().ino()
        );
        assert!(working_object.join("empty").is_dir());
        assert_eq!(
            fs::read_link(working_object.join("tool")).unwrap(),
            Path::new("bin/tool")
        );
        assert_eq!(
            fs::metadata(working_object.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn imports_fs_tree_manifest_only_after_hardlinking_complete_closure() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let (secondary, object_hash, file_hash) = fs_tree_object(&secondary_root);
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);

        assert_eq!(
            source.import_object(&working, object_hash).unwrap(),
            ContentImportOutcome::Imported
        );
        let secondary_file = secondary.fs_file_path_unchecked(file_hash);
        let working_file = working.fs_file_path_unchecked(file_hash);
        assert_eq!(
            fs::metadata(&secondary_file).unwrap().ino(),
            fs::metadata(&working_file).unwrap().ino()
        );
        assert_eq!(fs::read(working_file).unwrap(), b"fs-tree payload\n");
        assert!(working.object_path(object_hash).unwrap().is_some());
    }

    #[test]
    fn existing_manifest_repairs_its_missing_working_fs_file_from_secondary() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let (secondary, object_hash, file_hash) = fs_tree_object(&secondary_root);
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);
        source.import_object(&working, object_hash).unwrap();
        let working_file = working.fs_file_path_unchecked(file_hash);
        fs::remove_file(&working_file).unwrap();

        assert_eq!(
            source.import_object(&working, object_hash).unwrap(),
            ContentImportOutcome::AlreadyPresent
        );
        assert_eq!(
            fs::metadata(secondary.fs_file_path_unchecked(file_hash))
                .unwrap()
                .ino(),
            fs::metadata(working_file).unwrap().ino()
        );
    }

    #[test]
    fn missing_fs_file_does_not_publish_manifest_object() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let (secondary, object_hash, file_hash) = fs_tree_object(&secondary_root);
        fs::remove_file(secondary.fs_file_path_unchecked(file_hash)).unwrap();
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);

        let error = source.import_object(&working, object_hash).unwrap_err();
        assert!(error.to_string().contains("inspect fs-file"), "{error}");
        assert!(working.object_path(object_hash).unwrap().is_none());
    }

    #[test]
    fn noncanonical_fs_file_timestamp_is_rejected_without_publication() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let (secondary, object_hash, file_hash) = fs_tree_object(&secondary_root);
        let fs_file = secondary.fs_file_path_unchecked(file_hash);
        let file = OpenOptions::new().read(true).open(&fs_file).unwrap();
        file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(123)))
            .unwrap();
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);

        let error = source.import_object(&working, object_hash).unwrap_err();
        assert!(error.to_string().contains("noncanonical mtime"), "{error}");
        assert!(working.object_path(object_hash).unwrap().is_none());
        assert!(!working.fs_file_path_unchecked(file_hash).exists());
    }

    #[test]
    fn mismatched_named_object_is_rejected_and_staging_is_removed() {
        let temp = tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        let working_root = temp.path().join("working");
        let (secondary, _build, _reuse, actual_hash) = populated_store(&secondary_root);
        let expected_hash = ObjectHash::from_str(&"a".repeat(64)).unwrap();
        fs::hard_link(
            secondary.object_path(actual_hash).unwrap().unwrap(),
            secondary.object_path_unchecked(expected_hash),
        )
        .unwrap();
        let working = empty_store(&working_root);
        let source = host_content_source(&secondary_root);

        let error = source.import_object(&working, expected_hash).unwrap_err();
        assert!(
            error.to_string().contains("object hash mismatch"),
            "{error}"
        );
        assert!(working.object_path(expected_hash).unwrap().is_none());
        assert!(
            fs::read_dir(working.root().join(crate::store::OBJECTS_DIR))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".bobr-secondary-import-"))
        );
    }

    #[test]
    fn same_store_cannot_be_its_own_hardlink_source() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        let (store, _build, _reuse, object_hash) = populated_store(&root);
        let source = host_content_source(&root);

        let error = source.import_object(&store, object_hash).unwrap_err();
        assert!(error.to_string().contains("is the working store"));
    }

    #[test]
    fn secondary_runtime_registry_contains_fs_file_hardlink_function() {
        let functions = crate::runtime_functions();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name(), "secondary-hardlink-fs-files");
    }
}
