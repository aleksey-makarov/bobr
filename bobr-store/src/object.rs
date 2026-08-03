use crate::record::ObjectRecordSchemaV4;
use crate::{ObjectRecord, Store, StoreError};
use bobr_core::fsutil as private_fs;
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use fsobj_hash::hash_path;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PENDING_IMPORT: AtomicU64 = AtomicU64::new(0);

/// Imports a staged filesystem object into the store.
///
/// The object hash is computed from `staged_path`, then the staged path is
/// renamed into the store's legacy object directory. If an object with the same
/// hash already exists, the staged path is removed and the existing object is
/// reused.
///
/// `staged_path` is consumed on success. It may also be removed when the store
/// already contains the object.
pub(crate) fn import_object(store: &Store, staged_path: &Path) -> Result<ObjectHash, StoreError> {
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

    publish_staged_object(store, staged_path, &destination)?;

    Ok(object_hash)
}

fn publish_staged_object(
    store: &Store,
    staged_path: &Path,
    destination: &Path,
) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(staged_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to inspect staged object '{}': {error}",
            staged_path.display()
        ))
    })?;
    let mode = metadata.permissions().mode() & 0o7777;
    if metadata.is_dir() && mode & 0o222 == 0 {
        return publish_read_only_directory(store, staged_path, destination, mode);
    }

    if let Err(error) = fs::rename(staged_path, destination) {
        if destination.exists() {
            private_fs::remove_path_force(staged_path).map_err(crate::error::map_fsutil_error)?;
            return Ok(());
        }
        return Err(import_rename_error(staged_path, destination, error));
    }

    Ok(())
}

fn publish_read_only_directory(
    store: &Store,
    staged_path: &Path,
    destination: &Path,
    original_mode: u32,
) -> Result<(), StoreError> {
    // Moving a directory between parents updates its `..` entry. Linux
    // therefore requires write permission on the directory itself, even when
    // both parents are writable. Temporarily make only the staging root
    // writable and move it under a hidden name in objects/. The original mode
    // is restored before the same-directory rename publishes the hash path.
    let pending = next_pending_import_path(store)?;
    set_mode(staged_path, original_mode | 0o200).map_err(|error| {
        StoreError::Io(format!(
            "failed to prepare read-only staged object '{}' for import: {error}",
            staged_path.display()
        ))
    })?;

    if let Err(error) = fs::rename(staged_path, &pending) {
        return Err(import_error_with_mode_restore(
            staged_path,
            destination,
            original_mode,
            error,
        ));
    }

    if let Err(error) = set_mode(&pending, original_mode) {
        let rollback = rollback_pending_import(&pending, staged_path, original_mode);
        return Err(StoreError::Io(format!(
            "failed to restore read-only mode on pending object '{}': {error}; {rollback}",
            pending.display()
        )));
    }

    if let Err(error) = fs::rename(&pending, destination) {
        if destination.exists() {
            private_fs::remove_path_force(&pending).map_err(crate::error::map_fsutil_error)?;
            return Ok(());
        }
        let rollback = rollback_pending_import(&pending, staged_path, original_mode);
        return Err(StoreError::Io(format!(
            "{}; {rollback}",
            import_rename_error(staged_path, destination, error)
        )));
    }

    Ok(())
}

fn next_pending_import_path(store: &Store) -> Result<PathBuf, StoreError> {
    loop {
        let serial = NEXT_PENDING_IMPORT.fetch_add(1, Ordering::Relaxed);
        let path = store
            .objects_dir()
            .join(format!(".bobr-import-{}-{serial}", std::process::id()));
        match path.try_exists() {
            Ok(false) => return Ok(path),
            Ok(true) => {}
            Err(error) => {
                return Err(StoreError::Io(format!(
                    "failed to inspect pending object path '{}': {error}",
                    path.display()
                )));
            }
        }
    }
}

fn rollback_pending_import(pending: &Path, staged_path: &Path, original_mode: u32) -> String {
    if let Err(error) = set_mode(pending, original_mode | 0o200) {
        return format!(
            "failed to make pending object '{}' writable for rollback: {error}",
            pending.display()
        );
    }
    if let Err(error) = fs::rename(pending, staged_path) {
        return format!(
            "failed to roll pending object '{}' back to '{}': {error}",
            pending.display(),
            staged_path.display()
        );
    }
    match set_mode(staged_path, original_mode) {
        Ok(()) => "staged object was restored".to_string(),
        Err(error) => format!(
            "staged object was restored to '{}' but its mode could not be restored: {error}",
            staged_path.display()
        ),
    }
}

fn import_error_with_mode_restore(
    staged_path: &Path,
    destination: &Path,
    original_mode: u32,
    error: io::Error,
) -> StoreError {
    match set_mode(staged_path, original_mode) {
        Ok(()) => import_rename_error(staged_path, destination, error),
        Err(restore_error) => StoreError::Io(format!(
            "{}; failed to restore mode on '{}': {restore_error}",
            import_rename_error(staged_path, destination, error),
            staged_path.display()
        )),
    }
}

fn import_rename_error(staged_path: &Path, destination: &Path, error: io::Error) -> StoreError {
    StoreError::Io(format!(
        "failed to import object '{}' -> '{}': {error}",
        staged_path.display(),
        destination.display()
    ))
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
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
    run_id: &str,
) -> Result<ObjectHash, StoreError> {
    crate::validate_ref_name(object_ref_name)?;
    let object_hash = import_object(store, staged_path)?;
    let object_record = ObjectRecord {
        schema: ObjectRecordSchemaV4,
        build_key,
        object_hash,
        run_id: Some(run_id.to_string()),
        inputs,
    };
    crate::record::store_object_record(store, &object_record)?;
    crate::refs::store_reuse_ref(store, reuse_key, object_hash)?;
    crate::refs::store_build_handle_ref(store, build_key, object_hash)?;
    crate::refs::update_object_ref(store, object_ref_name, object_hash)?;
    Ok(object_hash)
}
