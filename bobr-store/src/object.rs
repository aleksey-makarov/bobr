use crate::fsutil as private_fs;
use crate::record::ObjectRecordSchemaV4;
use crate::{ObjectRecord, Store, StoreError};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use fsobj_hash::hash_path;
use std::fs;
use std::path::Path;

/// Imports a staged filesystem object into the store.
///
/// The object hash is computed from `staged_path`, then the staged path is
/// renamed into the store's legacy object directory. If an object with the same
/// hash already exists, the staged path is removed and the existing object is
/// reused.
///
/// `staged_path` is consumed on success. It may also be removed when the store
/// already contains the object.
pub fn import_object(store: &Store, staged_path: &Path) -> Result<ObjectHash, StoreError> {
    let object_hash = hash_path(staged_path).map_err(|error| {
        StoreError::Hashing(format!(
            "failed to hash staged object '{}': {error}",
            staged_path.display()
        ))
    })?;
    let destination = store.object_path_unchecked(object_hash);
    if destination.exists() {
        private_fs::remove_path_force(staged_path).map_err(crate::error::map_fsutil_error)?;
        return Ok(object_hash);
    }

    if let Err(error) = fs::rename(staged_path, &destination) {
        if destination.exists() {
            private_fs::remove_path_force(staged_path).map_err(crate::error::map_fsutil_error)?;
            return Ok(object_hash);
        }
        return Err(StoreError::Io(format!(
            "failed to import object '{}' -> '{}': {error}",
            staged_path.display(),
            destination.display()
        )));
    }

    Ok(object_hash)
}

/// Imports a staged object and records it as a newly materialized build.
///
/// The operation imports `staged_path`, stores the object record, writes the
/// reuse ref, writes the build handle ref, and updates `object-refs/<name>`
/// for the materialized object.
pub fn import_build(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    inputs: Vec<ObjectHash>,
    staged_path: &Path,
    object_ref_name: &str,
) -> Result<ObjectHash, StoreError> {
    crate::validate_ref_name(object_ref_name)?;
    let object_hash = import_object(store, staged_path)?;
    let object_record = ObjectRecord {
        schema: ObjectRecordSchemaV4,
        build_key,
        object_hash,
        run_id: Some(store.run_id().to_string()),
        inputs,
    };
    crate::record::store_object_record(store, &object_record)?;
    crate::refs::store_reuse_ref(store, reuse_key, object_hash)?;
    crate::refs::store_build_handle_ref(store, build_key, object_hash)?;
    crate::refs::update_object_ref(store, object_ref_name, object_hash)?;
    Ok(object_hash)
}
