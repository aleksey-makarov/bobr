use crate::{
    Build, BuildKey, PublishedBuild, ResultId, ResultRecord, ReuseKey, StoreError, StoreLayout,
};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

/// Returns the path of the build reference for `build_key`.
///
/// The path is under [`StoreLayout::builds`] and may or may not exist.
pub fn build_ref_path(layout: &StoreLayout, build_key: BuildKey) -> PathBuf {
    layout.builds.join(build_key.to_hex())
}

/// Returns the path of the JSON result record for `result_id`.
///
/// The path is under [`StoreLayout::results`] and has a `.json` suffix. The
/// function does not check whether the record currently exists.
pub fn result_path(layout: &StoreLayout, result_id: ResultId) -> PathBuf {
    layout.results.join(format!("{}.json", result_id.to_hex()))
}

/// Returns the path of the reuse reference for `reuse_key`.
///
/// The path is under [`StoreLayout::reuses`] and may or may not exist.
pub fn reuse_ref_path(layout: &StoreLayout, reuse_key: ReuseKey) -> PathBuf {
    layout.reuses.join(reuse_key.to_hex())
}

fn result_ref_target(result_id: ResultId) -> PathBuf {
    PathBuf::from("..")
        .join(crate::layout::RESULTS_DIR)
        .join(format!("{}.json", result_id.to_hex()))
}

fn parse_result_ref_target(
    ref_kind: &str,
    ref_path: &Path,
    target: &Path,
) -> Result<ResultId, StoreError> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StoreError::InvalidData(format!(
                "{ref_kind} ref '{}' points to invalid result target '{}'",
                ref_path.display(),
                target.display()
            ))
        })?;
    let result_id_str = file_name.strip_suffix(".json").ok_or_else(|| {
        StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to non-JSON result target '{}'",
            ref_path.display(),
            target.display()
        ))
    })?;
    let result_id = result_id_str.parse::<ResultId>().map_err(|error| {
        StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to invalid result id '{}' in target '{}': {error}",
            ref_path.display(),
            result_id_str,
            target.display()
        ))
    })?;
    let expected = result_ref_target(result_id);
    if target != expected {
        return Err(StoreError::InvalidData(format!(
            "{ref_kind} ref '{}' points to non-canonical result target '{}'; expected '{}'",
            ref_path.display(),
            target.display(),
            expected.display()
        )));
    }
    Ok(result_id)
}

/// Stores or replaces the build reference for `build_key`.
///
/// Build refs are symlinks pointing to result records through canonical
/// relative targets. The replacement is performed through a temporary symlink
/// and rename.
pub fn store_build_handle_ref(
    layout: &StoreLayout,
    build_key: BuildKey,
    result_id: ResultId,
) -> Result<(), StoreError> {
    let target = result_ref_target(result_id);
    replace_symlink(&target, &build_ref_path(layout, build_key))
}

/// Stores or replaces the reuse reference for `reuse_key`.
///
/// Reuse refs are symlinks pointing to result records through canonical
/// relative targets. The replacement is performed through a temporary symlink
/// and rename.
pub fn store_reuse_ref(
    layout: &StoreLayout,
    reuse_key: ReuseKey,
    result_id: ResultId,
) -> Result<(), StoreError> {
    let target = result_ref_target(result_id);
    replace_symlink(&target, &reuse_ref_path(layout, reuse_key))
}

/// Loads the published build reached by a build key.
///
/// Returns `Ok(None)` when the build ref does not exist. Existing refs must be
/// canonical symlinks to result records, and the referenced result must point to
/// an existing object in the store.
pub fn load_build_handle(
    layout: &StoreLayout,
    build_key: BuildKey,
) -> Result<Option<PublishedBuild>, StoreError> {
    let build_ref_path = build_ref_path(layout, build_key);
    if !build_ref_path.exists() && !build_ref_path.is_symlink() {
        return Ok(None);
    }

    let target = fs::read_link(&build_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read build ref '{}': {error}",
            build_ref_path.display()
        ))
    })?;
    let result_id = parse_result_ref_target("build", &build_ref_path, &target)?;
    let result = crate::record::load_result_record(layout, result_id)?.ok_or_else(|| {
        StoreError::InvalidData(format!(
            "build ref '{}' points to missing result '{}'",
            build_ref_path.display(),
            result_id
        ))
    })?;
    let object_path = crate::object::object_path(layout, result.object_hash);
    if !object_path.exists() {
        return Err(StoreError::Io(format!(
            "result '{}' points to missing object '{}'",
            result_id,
            object_path.display()
        )));
    }
    Ok(Some(PublishedBuild {
        build: crate::record::build_from_result(build_key, &result),
        result,
        object_path,
    }))
}

/// Loads the reusable result reached by a reuse key.
///
/// Returns `Ok(None)` when the reuse ref does not exist. Existing refs must be
/// canonical symlinks to result records.
pub fn load_reuse_record(
    layout: &StoreLayout,
    reuse_key: ReuseKey,
) -> Result<Option<ResultRecord>, StoreError> {
    let reuse_ref_path = reuse_ref_path(layout, reuse_key);
    if !reuse_ref_path.exists() && !reuse_ref_path.is_symlink() {
        return Ok(None);
    }

    let target = fs::read_link(&reuse_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read reuse ref '{}': {error}",
            reuse_ref_path.display()
        ))
    })?;
    let result_id = parse_result_ref_target("reuse", &reuse_ref_path, &target)?;
    crate::record::load_result_record(layout, result_id)
}

/// Loads the public build handle for `build_key`.
///
/// This is a narrower view of [`load_build_handle`] that returns only the
/// serializable [`Build`] value.
pub fn load_public_build(
    layout: &StoreLayout,
    build_key: BuildKey,
) -> Result<Option<Build>, StoreError> {
    Ok(load_build_handle(layout, build_key)?.map(|published| published.build))
}

/// Publishes a result under a public output name.
///
/// The current object and result refs for `output_name` are replaced with refs
/// to `result`. If the output name already points at a different result, the
/// previous refs are preserved as timestamped generation refs before the
/// current refs are updated.
///
/// Output names must be non-empty and contain only ASCII letters, digits, `.`,
/// `_`, or `-`.
pub fn publish_result_refs(
    layout: &StoreLayout,
    output_name: &str,
    result: &ResultRecord,
) -> Result<(), StoreError> {
    validate_output_name(output_name)?;

    let current_result_ref_path = layout.result_refs.join(format!("{output_name}.json"));
    let current_object_ref_path = layout.object_refs.join(output_name);
    let result_id = result.result_id();

    if let Some(current) = load_current_publication(layout, output_name)?
        && current.result.result_id() != result_id
    {
        let generation_name =
            allocate_generation_name(layout, output_name, &generation_suffix(&current)?)?;

        if let Some(target) = current.result_target {
            create_generation_ref(
                &target,
                &layout.result_refs.join(format!("{generation_name}.json")),
            )?;
        }
        if let Some(target) = current.object_target {
            create_generation_ref(&target, &layout.object_refs.join(&generation_name))?;
        }
    }

    let object_ref_target = object_ref_target_for_result(result)?;
    replace_symlink(&object_ref_target, &current_object_ref_path)?;

    let target = result_ref_target(result_id);
    replace_symlink(&target, &current_result_ref_path)?;
    Ok(())
}

/// Publishes the result contained in a fully resolved build.
///
/// This is a convenience wrapper around [`publish_result_refs`].
pub fn publish_refs(
    layout: &StoreLayout,
    output_name: &str,
    published: &PublishedBuild,
) -> Result<(), StoreError> {
    publish_result_refs(layout, output_name, &published.result)
}

fn object_ref_target_for_result(result: &ResultRecord) -> Result<PathBuf, StoreError> {
    let object_hash = result.object_hash.to_hex();
    Ok(PathBuf::from("..")
        .join(crate::layout::OBJECTS_DIR)
        .join(&object_hash))
}

fn validate_output_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(StoreError::InvalidInput(
            "output name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(StoreError::InvalidInput(format!(
            "invalid output name '{name}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(StoreError::InvalidInput(format!(
            "invalid output name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct CurrentPublication {
    result: ResultRecord,
    result_path: PathBuf,
    result_target: Option<PathBuf>,
    object_target: Option<PathBuf>,
}

pub(crate) fn load_current_publication(
    layout: &StoreLayout,
    output_name: &str,
) -> Result<Option<CurrentPublication>, StoreError> {
    let result_ref_path = layout.result_refs.join(format!("{output_name}.json"));
    if !result_ref_path.exists() && !result_ref_path.is_symlink() {
        return Ok(None);
    }

    let result_target = fs::read_link(&result_ref_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read current result ref '{}': {error}",
            result_ref_path.display()
        ))
    })?;
    let result_id = parse_result_ref_target("current result", &result_ref_path, &result_target)?;
    let result = crate::record::load_result_record(layout, result_id)?.ok_or_else(|| {
        StoreError::InvalidData(format!(
            "current result ref '{}' points to missing result '{}'",
            result_ref_path.display(),
            result_id
        ))
    })?;

    let object_ref_path = layout.object_refs.join(output_name);
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
        result_path: result_path(layout, result.result_id()),
        result,
        result_target: Some(result_target),
        object_target,
    }))
}

pub(crate) fn generation_suffix(current: &CurrentPublication) -> Result<String, StoreError> {
    if let Some(created_at) = &current.result.created_at {
        return human_timestamp_from_rfc3339(created_at);
    }

    let modified = fs::metadata(&current.result_path)
        .map_err(|error| {
            StoreError::Io(format!(
                "failed to stat result record '{}' for generation timestamp: {error}",
                current.result_path.display()
            ))
        })?
        .modified()
        .map_err(|error| {
            StoreError::Io(format!(
                "failed to read mtime for result record '{}': {error}",
                current.result_path.display()
            ))
        })?;
    let parsed = OffsetDateTime::from(modified);
    human_timestamp_from_datetime(parsed)
}

pub(crate) fn human_timestamp_from_rfc3339(value: &str) -> Result<String, StoreError> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        StoreError::InvalidData(format!(
            "invalid result record created_at '{value}': {error}"
        ))
    })?;
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
    layout: &StoreLayout,
    output_name: &str,
    suffix: &str,
) -> Result<String, StoreError> {
    for counter in 1..1000 {
        let candidate = if counter == 1 {
            format!("{output_name}.{suffix}")
        } else {
            format!("{output_name}.{suffix}.{counter}")
        };
        let result_path = layout.result_refs.join(format!("{candidate}.json"));
        let object_path = layout.object_refs.join(&candidate);
        if !(result_path.exists()
            || result_path.is_symlink()
            || object_path.exists()
            || object_path.is_symlink())
        {
            return Ok(candidate);
        }
    }

    Err(StoreError::Io(format!(
        "failed to allocate generation ref name for '{output_name}.{suffix}'"
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
