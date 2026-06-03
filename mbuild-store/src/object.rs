use crate::fsutil as private_fs;
use crate::{CasError, StoreLayout};
use fsobj_hash::{ObjectHash, hash_path};
use std::fs;
use std::path::Path;

pub fn object_path(layout: &StoreLayout, object_hash: ObjectHash) -> std::path::PathBuf {
    layout.objects.join(object_hash.to_hex())
}

pub fn import_object(layout: &StoreLayout, staged_path: &Path) -> Result<ObjectHash, CasError> {
    import_object_with_hash(layout, staged_path, None)
}

pub(crate) fn import_object_with_hash(
    layout: &StoreLayout,
    staged_path: &Path,
    object_hash: Option<ObjectHash>,
) -> Result<ObjectHash, CasError> {
    let precomputed = object_hash.is_some();
    let object_hash = match object_hash {
        Some(object_hash) => object_hash,
        None => hash_path(staged_path).map_err(|error| {
            CasError::Hashing(format!(
                "failed to hash staged object '{}': {error}",
                staged_path.display()
            ))
        })?,
    };
    let destination = layout.objects.join(object_hash.to_hex());
    if destination.exists() {
        if !precomputed {
            remove_path_force(staged_path)?;
        }
        return Ok(object_hash);
    }

    if let Err(error) = fs::rename(staged_path, &destination) {
        if destination.exists() {
            if !precomputed {
                remove_path_force(staged_path)?;
            }
            return Ok(object_hash);
        }
        return Err(CasError::Io(format!(
            "failed to import object '{}' -> '{}': {error}",
            staged_path.display(),
            destination.display()
        )));
    }

    Ok(object_hash)
}

pub(crate) fn remove_path_force(path: &Path) -> Result<(), CasError> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CasError::Io(format!(
            "failed to inspect path '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_dir() {
        private_fs::remove_dir_force(path).map_err(crate::error::map_fsutil_error)
    } else {
        fs::remove_file(path).map_err(|error| {
            CasError::Io(format!(
                "failed to remove file '{}': {error}",
                path.display()
            ))
        })
    }
}
