use crate::StoreError;
use crate::fsutil as private_fs;
use crate::{BuildKey, ResultId, ReuseKey};
use fsobj_hash::ObjectHash;
use serde_json::json;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

pub(crate) const OBJECTS_DIR: &str = "objects";
pub(crate) const BUILDS_DIR: &str = "builds";
pub(crate) const RESULTS_DIR: &str = "results";
pub(crate) const REUSES_DIR: &str = "reuses";
pub(crate) const OBJECT_REFS_DIR: &str = "object-refs";
pub(crate) const RESULT_REFS_DIR: &str = "result-refs";
pub(crate) const FS_FILES_DIR: &str = "fs-files";
pub(crate) const FS_TREES_DIR: &str = "fs-trees";
pub(crate) const BUILDER_STATE_DIR: &str = "builder-state";
pub(crate) const SOURCE_STATE_DIR: &str = "source-state";
pub(crate) const QUARANTINE_DIR: &str = "quarantine";
pub(crate) const LOGS_DIR: &str = "logs";
pub(crate) const RUNS_DIR: &str = "runs";

/// Store-owned directories used by a build run logger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRunLogLocations {
    /// Directory where event log files for complete build runs are created.
    pub event_logs_dir: PathBuf,
    /// Root directory for builder state and per-builder raw logs.
    pub builder_state_dir: PathBuf,
}

/// Store-owned workspace paths for a builder invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreBuilderWorkspace {
    /// Persistent state directory for this builder tag.
    pub state_dir: PathBuf,
    /// Temporary directory for this concrete build key.
    pub temp_dir: PathBuf,
}

/// Store-owned workspace paths for source origin materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSourceWorkspace {
    /// Temporary root passed to the source origin implementation.
    pub temp_dir: PathBuf,
}

/// Request to move a store-owned temporary path into quarantine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreTempQuarantineRequest {
    /// Existing store-owned temporary path to quarantine.
    pub temp_path: PathBuf,
    /// Builder tag associated with the temporary path.
    pub builder_tag: String,
    /// Build key associated with the temporary path.
    pub build_key: BuildKey,
    /// Human-readable reason stored in quarantine metadata.
    pub reason: String,
}

/// Result of quarantining a store-owned temporary path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedStoreTemp {
    /// Final quarantine path.
    pub path: PathBuf,
    /// Metadata write failure, when the temp path was moved but metadata failed.
    pub metadata_error: Option<String>,
}

/// Immutable handle to an `mbuild` store.
///
/// `Store` is the primary public interface for paths and operations that belong
/// to a store. It stores only the root path; all layout paths are derived from
/// the root through methods, so callers cannot mutate the store layout fields.
///
/// A `Store` is cloneable and thread-safe. Cloning copies the root path handle,
/// not store data.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Creates or initializes a store layout under an existing root directory.
    ///
    /// `root` must be an absolute path to an existing directory. Missing store
    /// subdirectories are created. Existing store subdirectories must be
    /// directories. The function does not validate existing records or
    /// references inside those directories.
    pub fn create(root: &Path) -> Result<Self, StoreError> {
        validate_root(root)?;
        let store = Self {
            root: root.to_path_buf(),
        };
        store.ensure()?;
        Ok(store)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the content-addressed legacy object directory.
    pub(crate) fn objects_dir(&self) -> PathBuf {
        self.root.join(OBJECTS_DIR)
    }

    /// Returns the build-key reference directory.
    pub(crate) fn builds_dir(&self) -> PathBuf {
        self.root.join(BUILDS_DIR)
    }

    /// Returns the reuse-key reference directory.
    pub(crate) fn reuses_dir(&self) -> PathBuf {
        self.root.join(REUSES_DIR)
    }

    /// Returns the JSON result record directory.
    pub(crate) fn results_dir(&self) -> PathBuf {
        self.root.join(RESULTS_DIR)
    }

    /// Returns the public object reference directory.
    pub(crate) fn object_refs_dir(&self) -> PathBuf {
        self.root.join(OBJECT_REFS_DIR)
    }

    /// Returns the public result reference directory.
    pub(crate) fn result_refs_dir(&self) -> PathBuf {
        self.root.join(RESULT_REFS_DIR)
    }

    /// Returns the content-addressed future fs-file object directory.
    pub fn fs_files_dir(&self) -> PathBuf {
        self.root.join(FS_FILES_DIR)
    }

    /// Returns the future fs-tree manifest directory.
    pub fn fs_trees_dir(&self) -> PathBuf {
        self.root.join(FS_TREES_DIR)
    }

    fn builder_state_dir(&self) -> PathBuf {
        self.root.join(BUILDER_STATE_DIR)
    }

    fn source_state_dir(&self) -> PathBuf {
        self.root.join(SOURCE_STATE_DIR)
    }

    fn quarantine_dir(&self) -> PathBuf {
        self.root.join(QUARANTINE_DIR)
    }

    fn run_logs_dir(&self) -> PathBuf {
        self.root.join(LOGS_DIR).join(RUNS_DIR)
    }

    /// Returns the canonical path of an imported legacy object.
    ///
    /// The path is `<store>/objects/<64-lowercase-object-hash>`. The function
    /// does not check whether the object currently exists.
    pub fn object_path(&self, object_hash: ObjectHash) -> PathBuf {
        self.objects_dir().join(object_hash.to_hex())
    }

    /// Returns the path of the build reference for `build_key`.
    ///
    /// The path is under the build-key reference directory and may or may not exist.
    pub(crate) fn build_ref_path(&self, build_key: BuildKey) -> PathBuf {
        self.builds_dir().join(build_key.to_hex())
    }

    /// Returns the path of the reuse reference for `reuse_key`.
    ///
    /// The path is under the reuse-key reference directory and may or may not exist.
    pub(crate) fn reuse_ref_path(&self, reuse_key: ReuseKey) -> PathBuf {
        self.reuses_dir().join(reuse_key.to_hex())
    }

    /// Returns the path of the JSON result record for `result_id`.
    ///
    /// The path is under the JSON result record directory and has a `.json` suffix. The
    /// function does not check whether the record currently exists.
    pub(crate) fn result_record_path(&self, result_id: ResultId) -> PathBuf {
        self.results_dir()
            .join(format!("{}.json", result_id.to_hex()))
    }

    fn ensure(&self) -> Result<(), StoreError> {
        ensure_store_dir(&self.objects_dir(), "objects")?;
        ensure_store_dir(&self.builds_dir(), "builds")?;
        ensure_store_dir(&self.reuses_dir(), "reuses")?;
        ensure_store_dir(&self.results_dir(), "results")?;
        ensure_store_dir(&self.object_refs_dir(), "object-refs")?;
        ensure_store_dir(&self.result_refs_dir(), "result-refs")?;
        ensure_store_dir(&self.fs_files_dir(), "fs-files")?;
        ensure_store_dir(&self.fs_trees_dir(), "fs-trees")?;
        Ok(())
    }
}

/// Returns store-owned directories needed by a build run logger.
pub fn run_log_locations(store: &Store) -> StoreRunLogLocations {
    StoreRunLogLocations {
        event_logs_dir: store.run_logs_dir(),
        builder_state_dir: store.builder_state_dir(),
    }
}

/// Returns store-owned workspace paths for a builder invocation.
pub fn builder_workspace(
    store: &Store,
    builder_tag: &str,
    build_key: BuildKey,
) -> StoreBuilderWorkspace {
    let state_dir = store
        .builder_state_dir()
        .join(builder_tag.to_ascii_lowercase());
    let temp_dir = state_dir.join("tmp").join(build_key.to_hex());
    StoreBuilderWorkspace {
        state_dir,
        temp_dir,
    }
}

/// Creates the builder state directory and returns builder workspace paths.
///
/// The temporary directory itself is not recreated here because runtime cleanup
/// policy may quarantine stale sandbox temp directories before creating it.
pub fn prepare_builder_workspace(
    store: &Store,
    builder_tag: &str,
    build_key: BuildKey,
) -> Result<StoreBuilderWorkspace, StoreError> {
    let workspace = builder_workspace(store, builder_tag, build_key);
    fs::create_dir_all(&workspace.state_dir).map_err(|error| {
        StoreError::Io(format!(
            "failed to create builder state directory '{}': {error}",
            workspace.state_dir.display()
        ))
    })?;
    Ok(workspace)
}

/// Returns store-owned workspace paths for source origin materialization.
pub fn source_workspace(store: &Store, key: BuildKey) -> StoreSourceWorkspace {
    StoreSourceWorkspace {
        temp_dir: store.source_state_dir().join("tmp").join(key.to_hex()),
    }
}

/// Recreates the source origin temporary directory and returns its paths.
pub fn prepare_source_workspace(
    store: &Store,
    key: BuildKey,
) -> Result<StoreSourceWorkspace, StoreError> {
    let workspace = source_workspace(store, key);
    recreate_store_temp_dir_force(store, &workspace.temp_dir)?;
    Ok(workspace)
}

/// Moves a store-owned temporary path into the store quarantine directory.
pub fn quarantine_store_temp(
    store: &Store,
    request: StoreTempQuarantineRequest,
) -> Result<QuarantinedStoreTemp, StoreError> {
    validate_store_temp_dir(store, &request.temp_path)?;
    let name = request
        .temp_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StoreError::InvalidInput(format!(
                "invalid temp path '{}'",
                request.temp_path.display()
            ))
        })?;
    let quarantine_dir = store.quarantine_dir();
    fs::create_dir_all(&quarantine_dir).map_err(|error| {
        StoreError::Io(format!(
            "failed to create quarantine directory '{}': {error}",
            quarantine_dir.display()
        ))
    })?;
    let stamp = current_epoch_nanos()?;
    let timestamp = human_quarantine_timestamp(stamp)?;

    for counter in 1..1000 {
        let suffix = if counter == 1 {
            timestamp.clone()
        } else {
            format!("{timestamp}.{counter}")
        };
        let target = quarantine_dir.join(format!(
            "{}-{}-{}-{name}",
            suffix,
            safe_quarantine_component(&request.builder_tag),
            request.build_key.to_hex(),
        ));
        if target.exists() || target.is_symlink() {
            continue;
        }
        match fs::rename(&request.temp_path, &target) {
            Ok(()) => {
                let metadata_error = write_quarantine_metadata(&target, &request, stamp)
                    .err()
                    .map(|error| error.to_string());
                return Ok(QuarantinedStoreTemp {
                    path: target,
                    metadata_error,
                });
            }
            Err(_) if target.exists() || target.is_symlink() => continue,
            Err(error) => {
                return Err(StoreError::Io(format!(
                    "failed to move temp path '{}' to '{}': {error}",
                    request.temp_path.display(),
                    target.display()
                )));
            }
        }
    }

    Err(StoreError::Io(format!(
        "failed to find quarantine target for temp path '{}' under '{}'",
        request.temp_path.display(),
        quarantine_dir.display()
    )))
}

/// Lists quarantined temporary paths, excluding their JSON metadata records.
pub fn list_quarantined_temps(store: &Store) -> Result<Vec<PathBuf>, StoreError> {
    let quarantine_dir = store.quarantine_dir();
    if !quarantine_dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(&quarantine_dir)
        .map_err(|error| {
            StoreError::Io(format!(
                "failed to read quarantine directory '{}': {error}",
                quarantine_dir.display()
            ))
        })?
        .map(|entry| {
            entry.map(|entry| entry.path()).map_err(|error| {
                StoreError::Io(format!(
                    "failed to read quarantine entry in '{}': {error}",
                    quarantine_dir.display()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.retain(|path| path.extension().and_then(|ext| ext.to_str()) != Some("json"));
    entries.sort();
    Ok(entries)
}

fn validate_root(root: &Path) -> Result<(), StoreError> {
    if !root.is_absolute() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be absolute: '{}'",
            root.display()
        )));
    }
    let metadata = fs::metadata(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StoreError::InvalidInput(format!("store root must exist: '{}'", root.display()))
        } else {
            StoreError::Io(format!(
                "failed to inspect store root '{}': {error}",
                root.display()
            ))
        }
    })?;
    if !metadata.is_dir() {
        return Err(StoreError::InvalidInput(format!(
            "store root must be a directory: '{}'",
            root.display()
        )));
    }
    Ok(())
}

fn ensure_store_dir(path: &Path, label: &str) -> Result<(), StoreError> {
    if path.exists() || path.is_symlink() {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            StoreError::Io(format!(
                "failed to inspect {label} directory '{}': {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_dir() {
            return Ok(());
        }
        return Err(StoreError::InvalidData(format!(
            "store {label} path '{}' is not a directory",
            path.display()
        )));
    }

    fs::create_dir_all(path).map_err(|error| {
        StoreError::Io(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

/// Removes and recreates a store-owned temporary directory.
///
/// The directory must be below the store root and include a `tmp` path
/// component. This guard keeps force-removal scoped to temporary directories
/// that belong to the store. The resulting directory exists and is empty.
pub fn recreate_store_temp_dir_force(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    validate_store_temp_dir(store, temp_dir)?;
    private_fs::recreate_empty_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

/// Removes a store-owned temporary directory if it exists.
///
/// The directory must be below the store root and include a `tmp` path
/// component. Missing directories are treated as success.
pub fn remove_store_temp_dir_force(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    validate_store_temp_dir(store, temp_dir)?;
    private_fs::remove_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

fn validate_store_temp_dir(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    if temp_dir == store.root() || !temp_dir.starts_with(store.root()) {
        return Err(StoreError::InvalidInput(format!(
            "store temp directory '{}' must be under store root '{}'",
            temp_dir.display(),
            store.root().display()
        )));
    }

    if !temp_dir
        .components()
        .any(|component| component.as_os_str() == OsStr::new("tmp"))
    {
        return Err(StoreError::InvalidInput(format!(
            "store temp directory '{}' must include a 'tmp' path component",
            temp_dir.display()
        )));
    }

    Ok(())
}

fn current_epoch_nanos() -> Result<u128, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| StoreError::Io(format!("system time before UNIX_EPOCH: {error}")))
}

fn human_quarantine_timestamp(stamp: u128) -> Result<String, StoreError> {
    let nanos = i128::try_from(stamp)
        .map_err(|_| StoreError::Io(format!("quarantine timestamp is out of range: {stamp}")))?;
    let parsed = OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|error| {
        StoreError::Io(format!(
            "failed to parse quarantine timestamp {stamp}: {error}"
        ))
    })?;
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local = parsed.to_offset(offset);
    let format = format_description!("[year repr:last_two][month][day][hour][minute][second]");
    local
        .format(&format)
        .map_err(|error| StoreError::Io(format!("failed to format quarantine timestamp: {error}")))
}

fn safe_quarantine_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn write_quarantine_metadata(
    target: &Path,
    request: &StoreTempQuarantineRequest,
    stamp: u128,
) -> Result<(), StoreError> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StoreError::InvalidInput(format!("invalid quarantine path '{}'", target.display()))
        })?;
    let metadata_path = target.with_file_name(format!("{file_name}.json"));
    let metadata = json!({
        "schema": "mbuild-quarantine-v1",
        "builder_tag": &request.builder_tag,
        "build_key": request.build_key.to_hex(),
        "original_path": request.temp_path.display().to_string(),
        "quarantine_path": target.display().to_string(),
        "reason": &request.reason,
        "created_at_unix_nanos": stamp.to_string(),
    });
    serde_json::to_vec_pretty(&metadata)
        .map_err(|error| {
            StoreError::InvalidData(format!("failed to encode quarantine metadata: {error}"))
        })
        .and_then(|bytes| {
            fs::write(&metadata_path, bytes).map_err(|error| {
                StoreError::Io(format!(
                    "failed to write quarantine metadata '{}': {error}",
                    metadata_path.display()
                ))
            })
        })
}
