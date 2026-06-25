use crate::fsutil as private_fs;
use crate::{Store, StoreError};
use bobr_core::ObjectHash;
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
    let destination = store.object_path(object_hash);
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
