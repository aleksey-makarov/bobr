use crate::identity::{BuildKey, ObjectHash, ReuseKey};
use crate::{Build, ObjectRecord, PublishedBuild, Store, StoreError, StoredObjectRecord};
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
) -> Result<Option<PublishedBuild>, StoreError> {
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
    let stored =
        crate::record::load_stored_object_record(store, object_hash)?.ok_or_else(|| {
            StoreError::InvalidData(format!(
                "build ref '{}' points to missing object record for object '{}'",
                build_ref_path.display(),
                object_hash
            ))
        })?;
    Ok(Some(PublishedBuild {
        build: crate::record::build_from_object_record(build_key, &stored.object_record),
        object_record: stored.object_record,
        object_path: stored.object_path,
    }))
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
) -> Result<Option<PublishedBuild>, StoreError> {
    let Some(object_record) = load_reuse_object_record(store, reuse_key)? else {
        return Ok(None);
    };
    let stored = crate::record::stored_object_record_from_record(store, object_record)?;
    store_build_handle_ref(store, build_key, stored.object_record.object_hash)?;
    Ok(Some(PublishedBuild {
        build: crate::record::build_from_object_record(build_key, &stored.object_record),
        object_record: stored.object_record,
        object_path: stored.object_path,
    }))
}

/// Loads the public build handle for `build_key`.
///
/// This is a narrower view of [`load_build_handle`] that returns only the
/// serializable [`Build`] value.
pub fn load_public_build(store: &Store, build_key: BuildKey) -> Result<Option<Build>, StoreError> {
    Ok(load_build_handle(store, build_key)?.map(|published| published.build))
}

/// Loads a publication by name.
///
/// Returns `Ok(None)` when neither publication ref exists for
/// `publication_name`. Existing publications must have both object-record and
/// object refs, both refs must use canonical targets, and the object ref must
/// point to the same object recorded by the object record.
pub fn load_publication(
    store: &Store,
    publication_name: &str,
) -> Result<Option<StoredObjectRecord>, StoreError> {
    validate_publication_name(publication_name)?;

    let object_record_ref_path = store
        .object_record_refs_dir()
        .join(format!("{publication_name}.json"));
    let object_ref_path = store.object_refs_dir().join(publication_name);
    let object_record_ref_exists =
        object_record_ref_path.exists() || object_record_ref_path.is_symlink();
    let object_ref_exists = object_ref_path.exists() || object_ref_path.is_symlink();

    match (object_record_ref_exists, object_ref_exists) {
        (false, false) => return Ok(None),
        (true, false) => {
            return Err(StoreError::InvalidData(format!(
                "publication '{publication_name}' has object record ref '{}' but missing object ref '{}'",
                object_record_ref_path.display(),
                object_ref_path.display()
            )));
        }
        (false, true) => {
            return Err(StoreError::InvalidData(format!(
                "publication '{publication_name}' has object ref '{}' but missing object record ref '{}'",
                object_ref_path.display(),
                object_record_ref_path.display()
            )));
        }
        (true, true) => {}
    }

    let object_record_target = fs::read_link(&object_record_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read publication object record ref '{}': {error}",
            object_record_ref_path.display()
        ))
    })?;
    let object_record_hash = parse_object_record_ref_target(
        "publication object record",
        &object_record_ref_path,
        &object_record_target,
    )?;
    let stored =
        crate::record::load_stored_object_record(store, object_record_hash)?.ok_or_else(|| {
            StoreError::InvalidData(format!(
                "publication object record ref '{}' points to missing object record for object '{}'",
                object_record_ref_path.display(),
                object_record_hash
            ))
        })?;

    let object_target = fs::read_link(&object_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read publication object ref '{}': {error}",
            object_ref_path.display()
        ))
    })?;
    let object_hash =
        parse_object_ref_target("publication object", &object_ref_path, &object_target)?;
    if object_hash != stored.object_record.object_hash {
        return Err(StoreError::InvalidData(format!(
            "publication '{publication_name}' object ref points to '{}' but object record points to '{}'",
            object_hash, stored.object_record.object_hash
        )));
    }

    Ok(Some(stored))
}

/// Publishes a stored object under a publication name.
///
/// The object record must already exist and must point to an existing object in
/// the store. The checked object record is returned to callers that need the resolved
/// record and object path.
pub fn publish_stored_object(
    store: &Store,
    publication_name: &str,
    object_hash: ObjectHash,
) -> Result<StoredObjectRecord, StoreError> {
    let stored =
        crate::record::load_stored_object_record(store, object_hash)?.ok_or_else(|| {
            StoreError::InvalidData(format!(
                "cannot publish missing object record for object '{}'",
                object_hash
            ))
        })?;
    publish_publication_refs(store, publication_name, &stored.object_record)?;
    Ok(stored)
}

/// Publishes an object record under a publication name.
///
/// The current object and object-record refs for `publication_name` are
/// replaced with refs to `object_record`. If the publication already points at
/// a different object record, the previous refs are preserved as timestamped
/// generation refs before the current refs are updated.
///
/// Publication names must be non-empty and contain only ASCII letters, digits,
/// `.`, `_`, or `-`.
pub(crate) fn publish_publication_refs(
    store: &Store,
    publication_name: &str,
    object_record: &ObjectRecord,
) -> Result<(), StoreError> {
    validate_publication_name(publication_name)?;

    let current_object_record_ref_path = store
        .object_record_refs_dir()
        .join(format!("{publication_name}.json"));
    let current_object_ref_path = store.object_refs_dir().join(publication_name);
    let object_hash = object_record.object_hash;

    if let Some(current) = load_current_publication(store, publication_name)?
        && current.object_record.object_hash != object_hash
    {
        let generation_name =
            allocate_generation_name(store, publication_name, &generation_suffix(&current)?)?;

        if let Some(target) = current.object_record_target {
            create_generation_ref(
                &target,
                &store
                    .object_record_refs_dir()
                    .join(format!("{generation_name}.json")),
            )?;
        }
        if let Some(target) = current.object_target {
            create_generation_ref(&target, &store.object_refs_dir().join(&generation_name))?;
        }
    }

    let object_ref_target = object_ref_target_for_record(object_record);
    replace_symlink(&object_ref_target, &current_object_ref_path)?;

    let target = object_record_ref_target(object_hash);
    replace_symlink(&target, &current_object_record_ref_path)?;
    Ok(())
}

fn object_ref_target_for_record(object_record: &ObjectRecord) -> PathBuf {
    object_ref_target(object_record.object_hash)
}

fn validate_publication_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(StoreError::InvalidInput(
            "publication name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(StoreError::InvalidInput(format!(
            "invalid publication name '{name}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(StoreError::InvalidInput(format!(
            "invalid publication name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct CurrentPublication {
    object_record: ObjectRecord,
    object_record_path: PathBuf,
    object_record_target: Option<PathBuf>,
    object_target: Option<PathBuf>,
}

pub(crate) fn load_current_publication(
    store: &Store,
    publication_name: &str,
) -> Result<Option<CurrentPublication>, StoreError> {
    let object_record_ref_path = store
        .object_record_refs_dir()
        .join(format!("{publication_name}.json"));
    if !object_record_ref_path.exists() && !object_record_ref_path.is_symlink() {
        return Ok(None);
    }

    let object_record_target = fs::read_link(&object_record_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read current object record ref '{}': {error}",
            object_record_ref_path.display()
        ))
    })?;
    let object_hash = parse_object_record_ref_target(
        "current object record",
        &object_record_ref_path,
        &object_record_target,
    )?;
    let object_record =
        crate::record::load_object_record(store, object_hash)?.ok_or_else(|| {
            StoreError::InvalidData(format!(
                "current object record ref '{}' points to missing object record for object '{}'",
                object_record_ref_path.display(),
                object_hash
            ))
        })?;

    let object_ref_path = store.object_refs_dir().join(publication_name);
    let object_target = if object_ref_path.exists() || object_ref_path.is_symlink() {
        Some(fs::read_link(&object_ref_path).map_err(|error| {
            StoreError::Io(format!(
                "failed to read current object ref '{}': {error}",
                object_ref_path.display()
            ))
        })?)
    } else {
        None
    };

    Ok(Some(CurrentPublication {
        object_record_path: store.object_record_path(object_record.object_hash),
        object_record,
        object_record_target: Some(object_record_target),
        object_target,
    }))
}

pub(crate) fn generation_suffix(current: &CurrentPublication) -> Result<String, StoreError> {
    if let Some(run_id) = &current.object_record.run_id {
        return Ok(run_id.clone());
    }

    let modified = fs::metadata(&current.object_record_path)
        .map_err(|error| {
            StoreError::Io(format!(
                "failed to stat object record '{}' for generation timestamp: {error}",
                current.object_record_path.display()
            ))
        })?
        .modified()
        .map_err(|error| {
            StoreError::Io(format!(
                "failed to read mtime for object record '{}': {error}",
                current.object_record_path.display()
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
    publication_name: &str,
    suffix: &str,
) -> Result<String, StoreError> {
    for counter in 1..1000 {
        let candidate = if counter == 1 {
            format!("{publication_name}.{suffix}")
        } else {
            format!("{publication_name}.{suffix}.{counter}")
        };
        let object_record_ref_path = store
            .object_record_refs_dir()
            .join(format!("{candidate}.json"));
        let object_path = store.object_refs_dir().join(&candidate);
        if !(object_record_ref_path.exists()
            || object_record_ref_path.is_symlink()
            || object_path.exists()
            || object_path.is_symlink())
        {
            return Ok(candidate);
        }
    }

    Err(StoreError::Io(format!(
        "failed to allocate generation ref name for '{publication_name}.{suffix}'"
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
