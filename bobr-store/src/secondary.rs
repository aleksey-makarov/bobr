//! Narrow read-only capabilities exposed by secondary stores.
//!
//! A trusted key index makes unverifiable build/reuse assertions. A content
//! source only reports self-verifiable content addressed by an already-known
//! object hash. Keeping the traits independent permits a small trusted index
//! to name content served by another, untrusted backend.

use crate::record::parse_object_record_value;
use crate::refs::parse_object_record_ref_target;
use crate::{ObjectRecord, ReadOnlyStore, StoreError};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::Path;

/// Trusted metadata resolving one build or reuse key to an object.
///
/// The canonical object record travels with the resolution so a later
/// promotion can recreate working-store metadata without asking the content
/// source to make trusted assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedResolution<K> {
    /// Build or reuse key that was resolved.
    pub key: K,
    /// Object named by the trusted mapping.
    pub object_hash: ObjectHash,
    /// Canonical record referenced by that mapping.
    pub record: ObjectRecord,
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
    /// Missing keys are omitted from the returned results. Malformed mappings or
    /// records fail the whole batch rather than being silently treated as
    /// misses.
    fn resolve_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<TrustedResolution<BuildKey>>, StoreError>;

    /// Resolves every available reuse key in one batch.
    ///
    /// Missing keys are omitted from the returned results. Malformed mappings or
    /// records fail the whole batch rather than being silently treated as
    /// misses.
    fn resolve_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<TrustedResolution<ReuseKey>>, StoreError>;
}

/// Read-only capability for content addressed by a known object hash.
///
/// This first-stage interface only performs batched availability discovery.
/// Staging and verified hardlink import are added by the next implementation
/// step without granting this capability access to trusted key mappings.
pub trait ContentSource: fmt::Debug + Send + Sync {
    /// Returns the subset of `hashes` whose top-level object payload exists.
    ///
    /// This does not claim that an fs-tree's referenced fs-files are complete;
    /// closure discovery belongs to the later staging/import operation.
    fn locate_objects(&self, hashes: &[ObjectHash]) -> Result<HashSet<ObjectHash>, StoreError>;
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
                && let Some((object_hash, record)) =
                    load_resolution("build", &self.store.build_ref_path(*key), &self.store)?
            {
                found.push(TrustedResolution {
                    key: *key,
                    object_hash,
                    record,
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
                && let Some((object_hash, record)) =
                    load_resolution("reuse", &self.store.reuse_ref_path(*key), &self.store)?
            {
                found.push(TrustedResolution {
                    key: *key,
                    object_hash,
                    record,
                });
            }
        }
        Ok(found)
    }
}

/// Hardlink-oriented content availability backed by a local read-only store.
///
/// This type does not expose key-index operations. The following implementation
/// step extends it with staging that requires hardlinks into a particular
/// same-filesystem working store; it will not silently fall back to copying.
#[derive(Debug, Clone)]
pub struct LocalHardlinkContentSource {
    store: ReadOnlyStore,
}

impl LocalHardlinkContentSource {
    /// Wraps a validated read-only local store as a hardlink content source.
    pub fn new(store: ReadOnlyStore) -> Self {
        Self { store }
    }

    /// Returns the underlying read-only store.
    pub fn store(&self) -> &ReadOnlyStore {
        &self.store
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
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
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
}

fn load_resolution(
    kind: &str,
    ref_path: &Path,
    store: &ReadOnlyStore,
) -> Result<Option<(ObjectHash, ObjectRecord)>, StoreError> {
    let metadata = match fs::symlink_metadata(ref_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
    let object_hash = parse_object_record_ref_target(kind, ref_path, &target)?;
    let record_path = store.object_record_path(object_hash);
    let bytes = fs::read(&record_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StoreError::InvalidData(format!(
                "{kind} ref '{}' points to missing object record for object '{}'",
                ref_path.display(),
                object_hash
            ))
        } else {
            StoreError::Io(format!(
                "failed to read secondary object record '{}': {error}",
                record_path.display()
            ))
        }
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::InvalidData(format!(
            "failed to parse secondary object record '{}': {error}",
            record_path.display()
        ))
    })?;
    let record = parse_object_record_value(object_hash, &value)?;
    Ok(Some((object_hash, record)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, import_build};
    use std::os::unix::fs::symlink;
    use std::str::FromStr;
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
        assert_eq!(builds[0].record.object_hash, object_hash);

        let reuses = index.resolve_reuses(&[reuse, reuse_key('4')]).unwrap();
        assert_eq!(reuses.len(), 1);
        assert_eq!(reuses[0].key, reuse);
        assert_eq!(reuses[0].object_hash, object_hash);
        assert_eq!(reuses[0].record.object_hash, object_hash);
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
    fn malformed_index_metadata_is_an_error_not_a_miss() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("store");
        let (store, build, _reuse, object_hash) = populated_store(&root);
        let ref_path = store.build_ref_path(build);
        fs::remove_file(&ref_path).unwrap();
        symlink("../objects/not-a-record", &ref_path).unwrap();
        let index = LocalTrustedKeyIndex::new(ReadOnlyStore::open(&root).unwrap());

        let error = index.resolve_builds(&[build]).unwrap_err();
        assert!(error.to_string().contains("non-JSON object record target"));

        fs::remove_file(&ref_path).unwrap();
        let canonical_target =
            Path::new("../object-records").join(format!("{}.json", object_hash.to_hex()));
        symlink(canonical_target, &ref_path).unwrap();
        fs::remove_file(store.object_record_path(object_hash)).unwrap();
        let error = index.resolve_builds(&[build]).unwrap_err();
        assert!(error.to_string().contains("missing object record"));
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
}
