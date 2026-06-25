use crate::{ObjectRecord, Store, StoreError, validate_ref_name};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

fn object_record_ref_target(object_hash: ObjectHash) -> PathBuf {
    PathBuf::from("..")
        .join(crate::store::OBJECT_RECORDS_DIR)
        .join(format!("{}.json", object_hash.to_hex()))
}

fn object_ref_target(object_hash: ObjectHash) -> PathBuf {
    PathBuf::from("..")
        .join(crate::store::OBJECTS_DIR)
        .join(object_hash.to_hex())
}

fn parse_object_record_ref_target(
    ref_kind: &str,
    ref_path: &Path,
    target: &Path,
) -> Result<ObjectHash, StoreError> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StoreError::InvalidData(format!(
                "{ref_kind} ref '{}' points to invalid object record target '{}'",
                ref_path.display(),
                target.display()
            ))
        })?;
    let object_hash_str = file_name.strip_suffix(".json").ok_or_else(|| {
        StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to non-JSON object record target '{}'",
            ref_path.display(),
            target.display()
        ))
    })?;
    let object_hash = object_hash_str.parse::<ObjectHash>().map_err(|error| {
        StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to invalid object hash '{}' in target '{}': {error}",
            ref_path.display(),
            object_hash_str,
            target.display()
        ))
    })?;
    let expected = object_record_ref_target(object_hash);
    if target != expected {
        return Err(StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to non-canonical object record target '{}'; expected '{}'",
            ref_path.display(),
            target.display(),
            expected.display()
        )));
    }
    Ok(object_hash)
}

fn parse_object_ref_target(
    ref_kind: &str,
    ref_path: &Path,
    target: &Path,
) -> Result<ObjectHash, StoreError> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StoreError::InvalidData(format!(
                "{ref_kind} ref '{}' points to invalid object target '{}'",
                ref_path.display(),
                target.display()
            ))
        })?;
    let object_hash = file_name.parse::<ObjectHash>().map_err(|error| {
        StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to invalid object hash '{}' in target '{}': {error}",
            ref_path.display(),
            file_name,
            target.display()
        ))
    })?;
    let expected = object_ref_target(object_hash);
    if target != expected {
        return Err(StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to non-canonical object target '{}'; expected '{}'",
            ref_path.display(),
            target.display(),
            expected.display()
        )));
    }
    Ok(object_hash)
}

/// Stores or replaces the build reference for `build_key`.
///
/// Build refs are symlinks pointing to object records through canonical
/// relative targets. The replacement is performed through a temporary symlink
/// and rename.
pub(crate) fn store_build_handle_ref(
    store: &Store,
    build_key: BuildKey,
    object_hash: ObjectHash,
) -> Result<(), StoreError> {
    let target = object_record_ref_target(object_hash);
    replace_symlink(&target, &store.build_ref_path(build_key))
}

/// Stores or replaces the reuse reference for `reuse_key`.
///
/// Reuse refs are symlinks pointing to object records through canonical
/// relative targets. The replacement is performed through a temporary symlink
/// and rename.
pub(crate) fn store_reuse_ref(
    store: &Store,
    reuse_key: ReuseKey,
    object_hash: ObjectHash,
) -> Result<(), StoreError> {
    let target = object_record_ref_target(object_hash);
    replace_symlink(&target, &store.reuse_ref_path(reuse_key))
}

/// Loads the published build reached by a build key.
///
/// Returns `Ok(None)` when the build ref does not exist. Existing refs must be
/// canonical symlinks to object records, and the referenced object record must point to
/// an existing object in the store.
pub fn load_build_handle(
    store: &Store,
    build_key: BuildKey,
) -> Result<Option<ObjectHash>, StoreError> {
    let build_ref_path = store.build_ref_path(build_key);
    if !build_ref_path.exists() && !build_ref_path.is_symlink() {
        return Ok(None);
    }

    let target = fs::read_link(&build_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read build ref '{}': {error}",
            build_ref_path.display()
        ))
    })?;
    let object_hash = parse_object_record_ref_target("build", &build_ref_path, &target)?;
    // Validate that the referenced object record and its object exist.
    crate::record::load_stored_object_record(store, object_hash)?.ok_or_else(|| {
        StoreError::InvalidData(format!(
            "build ref '{}' points to missing object record for object '{}'",
            build_ref_path.display(),
            object_hash
        ))
    })?;
    Ok(Some(object_hash))
}

/// Resolves the build reached by a build key and optionally updates its object ref.
///
/// This is the normal runtime-facing build-handle resolver. When `object_ref_name`
/// is provided, a successful hit also updates `object-refs/<name>` to point at
/// the resolved object.
pub fn resolve_build_handle(
    store: &Store,
    build_key: BuildKey,
    object_ref_name: Option<&str>,
) -> Result<Option<ObjectHash>, StoreError> {
    let Some(object_hash) = load_build_handle(store, build_key)? else {
        return Ok(None);
    };
    if let Some(name) = object_ref_name {
        update_object_ref(store, name, object_hash)?;
    }
    Ok(Some(object_hash))
}

/// Loads the reusable object record reached by a reuse key.
///
/// Returns `Ok(None)` when the reuse ref does not exist. Existing refs must be
/// canonical symlinks to object records.
pub(crate) fn load_reuse_object_record(
    store: &Store,
    reuse_key: ReuseKey,
) -> Result<Option<ObjectRecord>, StoreError> {
    let reuse_ref_path = store.reuse_ref_path(reuse_key);
    if !reuse_ref_path.exists() && !reuse_ref_path.is_symlink() {
        return Ok(None);
    }

    let target = fs::read_link(&reuse_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read reuse ref '{}': {error}",
            reuse_ref_path.display()
        ))
    })?;
    let object_hash = parse_object_record_ref_target("reuse", &reuse_ref_path, &target)?;
    let object_record =
        crate::record::load_object_record(store, object_hash)?.ok_or_else(|| {
            StoreError::InvalidData(format!(
                "reuse ref '{}' points to missing object record for object '{}'",
                reuse_ref_path.display(),
                object_hash
            ))
        })?;
    Ok(Some(object_record))
}

/// Resolves a reusable object record and repairs the build handle for `build_key`.
///
/// Returns `Ok(None)` when the reuse ref does not exist. Existing reuse refs
/// must point to an existing object record whose object exists in the store.
pub fn resolve_reuse_for_build(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    object_ref_name: Option<&str>,
) -> Result<Option<ObjectHash>, StoreError> {
    let Some(object_record) = load_reuse_object_record(store, reuse_key)? else {
        return Ok(None);
    };
    let stored = crate::record::stored_object_record_from_record(store, object_record)?;
    let object_hash = stored.object_record.object_hash;
    store_build_handle_ref(store, build_key, object_hash)?;
    if let Some(name) = object_ref_name {
        update_object_ref(store, name, object_hash)?;
    }
    Ok(Some(object_hash))
}

/// Updates the current object ref for `object_ref_name`.
///
/// If the current ref points at a different object, the previous symlink target
/// is preserved as an mtime-suffixed generation ref before the current ref is
/// replaced.
pub(crate) fn update_object_ref(
    store: &Store,
    object_ref_name: &str,
    object_hash: ObjectHash,
) -> Result<(), StoreError> {
    validate_ref_name(object_ref_name)?;

    let current_object_ref_path = store.object_refs_dir().join(object_ref_name);
    let new_target = object_ref_target(object_hash);

    match fs::symlink_metadata(&current_object_ref_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let current_target = fs::read_link(&current_object_ref_path).map_err(|error| {
                StoreError::Io(format!(
                    "failed to read current object ref '{}': {error}",
                    current_object_ref_path.display()
                ))
            })?;
            let current_hash = parse_object_ref_target(
                "current object",
                &current_object_ref_path,
                &current_target,
            )?;
            if current_hash == object_hash {
                return Ok(());
            }
            let suffix = generation_suffix_from_symlink_metadata(&metadata)?;
            let generation_name = allocate_generation_name(store, object_ref_name, &suffix)?;
            create_generation_ref(
                &current_target,
                &store.object_refs_dir().join(&generation_name),
            )?;
        }
        Ok(_) => {
            return Err(StoreError::InvalidData(format!(
                "object ref '{}' exists but is not a symlink",
                current_object_ref_path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(StoreError::Io(format!(
                "failed to inspect object ref '{}': {error}",
                current_object_ref_path.display()
            )));
        }
    }

    replace_symlink(&new_target, &current_object_ref_path)
}

fn generation_suffix_from_symlink_metadata(metadata: &fs::Metadata) -> Result<String, StoreError> {
    let modified = metadata.modified().map_err(|error| {
        StoreError::Io(format!(
            "failed to read object ref mtime for generation: {error}"
        ))
    })?;
    let parsed = OffsetDateTime::from(modified);
    human_timestamp_from_datetime(parsed)
}

fn human_timestamp_from_datetime(parsed: OffsetDateTime) -> Result<String, StoreError> {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local = parsed.to_offset(offset);
    let format = format_description!("[year repr:last_two][month][day][hour][minute][second]");
    local.format(&format).map_err(|error| {
        StoreError::InvalidData(format!("failed to format generation suffix: {error}"))
    })
}

fn allocate_generation_name(
    store: &Store,
    object_ref_name: &str,
    suffix: &str,
) -> Result<String, StoreError> {
    for counter in 1..1000 {
        let candidate = if counter == 1 {
            format!("{object_ref_name}.{suffix}")
        } else {
            format!("{object_ref_name}.{suffix}.{counter}")
        };
        let object_path = store.object_refs_dir().join(&candidate);
        if !(object_path.exists() || object_path.is_symlink()) {
            return Ok(candidate);
        }
    }

    Err(StoreError::Io(format!(
        "failed to allocate generation ref name for '{object_ref_name}.{suffix}'"
    )))
}

fn create_generation_ref(target: &Path, link_path: &Path) -> Result<(), StoreError> {
    if link_path.exists() || link_path.is_symlink() {
        return Err(StoreError::Io(format!(
            "ref generation collision at '{}'",
            link_path.display()
        )));
    }
    create_symlink(target, link_path)
}

pub(crate) fn replace_symlink(target: &Path, link_path: &Path) -> Result<(), StoreError> {
    if link_path.exists() || link_path.is_symlink() {
        let metadata = fs::symlink_metadata(link_path).map_err(|error| {
            StoreError::Io(format!(
                "failed to inspect existing ref '{}': {error}",
                link_path.display()
            ))
        })?;
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(link_path).map_err(|error| {
                StoreError::Io(format!(
                    "failed to remove existing ref directory '{}': {error}",
                    link_path.display()
                ))
            })?;
        }
    }

    let parent = link_path.parent().ok_or_else(|| {
        StoreError::Io(format!(
            "ref path '{}' has no parent directory",
            link_path.display()
        ))
    })?;
    let file_name = link_path.file_name().ok_or_else(|| {
        StoreError::Io(format!(
            "ref path '{}' has no file name",
            link_path.display()
        ))
    })?;
    let file_name = file_name.to_string_lossy();
    let pid = std::process::id();

    for attempt in 0..1000u32 {
        let temp_path = parent.join(format!(".{file_name}.tmp.{pid}.{attempt}"));
        if temp_path.exists() || temp_path.is_symlink() {
            continue;
        }
        create_symlink(target, &temp_path)?;
        match fs::rename(&temp_path, link_path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(StoreError::Io(format!(
                    "failed to replace ref '{}' with temporary symlink '{}': {error}",
                    link_path.display(),
                    temp_path.display()
                )));
            }
        }
    }

    Err(StoreError::Io(format!(
        "failed to allocate temporary ref symlink for '{}'",
        link_path.display()
    )))
}

fn create_symlink(target: &Path, link_path: &Path) -> Result<(), StoreError> {
    unix_fs::symlink(target, link_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to create ref symlink '{}' -> '{}': {error}",
            link_path.display(),
            target.display()
        ))
    })
}
