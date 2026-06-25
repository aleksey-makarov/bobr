use crate::object::import_object;
use crate::record::record_existing_source_object as record_existing_source_object_record;
use crate::{Store, StoreError};
use bobr_core::{BuildKey, ObjectHash};
use std::path::Path;

/// Outcome of importing a materialized source origin into the store.
#[derive(Debug, Clone)]
pub enum SourceImportOutcome {
    /// The materialized object matched the declared hash and was recorded.
    Matched(ObjectHash),
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
    object_ref_name: &str,
) -> Result<Option<ObjectHash>, StoreError> {
    crate::validate_ref_name(object_ref_name)?;
    if store.object_path(declared_hash)?.is_none() {
        return Ok(None);
    }

    record_existing_source_object_record(store, declared_hash)?;
    record_source_build_handle(store, declared_hash)?;
    crate::refs::update_object_ref(store, object_ref_name, declared_hash)?;
    Ok(Some(declared_hash))
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
    object_ref_name: &str,
) -> Result<SourceImportOutcome, StoreError> {
    crate::validate_ref_name(object_ref_name)?;
    let actual_hash = import_object(store, staged_path)?;
    if actual_hash != declared_hash {
        return Ok(SourceImportOutcome::Mismatched { actual_hash });
    }

    record_existing_source_object_record(store, declared_hash)?;
    record_source_build_handle(store, declared_hash)?;
    crate::refs::update_object_ref(store, object_ref_name, declared_hash)?;
    Ok(SourceImportOutcome::Matched(declared_hash))
}

fn record_source_build_handle(store: &Store, declared_hash: ObjectHash) -> Result<(), StoreError> {
    crate::refs::store_build_handle_ref(
        store,
        BuildKey::from_object_hash(declared_hash),
        declared_hash,
    )
}
