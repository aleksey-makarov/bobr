use crate::object::import_object;
use crate::record::{
    StoredObjectRecord, record_existing_source_object as record_existing_source_object_record,
};
use crate::{Store, StoreError};
use mbuild_core::{BuildKey, ObjectHash};
use std::path::Path;

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

/// Records an already-imported source object when it is present in the store.
///
/// If the object is missing, this returns `Ok(None)`. If it exists, this
/// idempotently writes the canonical object record and the source build handle
/// `builds/<object_hash>`.
pub fn record_existing_source_object(
    store: &Store,
    declared_hash: ObjectHash,
) -> Result<Option<StoredObjectRecord>, StoreError> {
    if !store.object_path(declared_hash).exists() {
        return Ok(None);
    }

    let stored = record_existing_source_object_record(store, declared_hash)?;
    record_source_build_handle(store, declared_hash)?;
    Ok(Some(stored))
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

    let stored = record_existing_source_object_record(store, declared_hash)?;
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
