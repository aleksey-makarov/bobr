use crate::identity::{BuildKey, ObjectHash};
use crate::object::import_object;
use crate::record::{StoredObjectRecord, load_stored_object_record, record_existing_source_object};
use crate::{Store, StoreError};
use std::path::Path;

/// Outcome of looking up the canonical store state for a declared source object.
#[derive(Debug, Clone)]
pub enum SourceLookup {
    /// The source is available through an existing canonical object record.
    Hit(StoredObjectRecord),
    /// Neither the canonical object record nor the declared object exists in the store.
    Missing,
}

/// Outcome of importing a materialized source origin into the store.
#[derive(Debug, Clone)]
pub enum SourceImportOutcome {
    /// The materialized object matched the declared hash and was recorded.
    Matched(StoredObjectRecord),
    /// The materialized object was imported, but it did not match the declared hash.
    Mismatched {
        /// Hash of the object that was actually imported.
        actual_hash: ObjectHash,
    },
}

/// Looks up the canonical object record for a declared source object.
///
/// If the object record is missing but the declared object already exists in
/// the store, this records the object as a canonical source object
/// idempotently and returns it as a hit. On every hit, this also records the
/// source build handle `builds/<object_hash>`.
pub fn lookup_source_object(
    store: &Store,
    declared_hash: ObjectHash,
) -> Result<SourceLookup, StoreError> {
    if let Some(stored) = load_stored_object_record(store, declared_hash)? {
        record_source_build_handle(store, declared_hash)?;
        return Ok(SourceLookup::Hit(stored));
    }

    if store.object_path(declared_hash).exists() {
        let stored = record_existing_source_object(store, declared_hash)?;
        record_source_build_handle(store, declared_hash)?;
        return Ok(SourceLookup::Hit(stored));
    }

    Ok(SourceLookup::Missing)
}

/// Imports a materialized source origin and records it if it matches the declaration.
///
/// The staged object is always imported into the store before the hash is
/// compared. On mismatch the imported actual object remains in the store, but
/// the canonical object record and source build handle for the declared hash
/// are not written.
pub fn import_source_object(
    store: &Store,
    declared_hash: ObjectHash,
    staged_path: &Path,
) -> Result<SourceImportOutcome, StoreError> {
    let actual_hash = import_object(store, staged_path)?;
    if actual_hash != declared_hash {
        return Ok(SourceImportOutcome::Mismatched { actual_hash });
    }

    let stored = record_existing_source_object(store, declared_hash)?;
    record_source_build_handle(store, declared_hash)?;
    Ok(SourceImportOutcome::Matched(stored))
}

fn record_source_build_handle(store: &Store, declared_hash: ObjectHash) -> Result<(), StoreError> {
    crate::refs::store_build_handle_ref(
        store,
        BuildKey::from_object_hash(declared_hash),
        declared_hash,
    )
}
