use crate::StoreError;
use crate::fs_tree::FsTree;
use crate::fsutil as private_fs;
use mbuild_core::{BuildKey, ObjectHash, ReuseKey};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

pub(crate) const OBJECTS_DIR: &str = "objects";
pub(crate) const BUILDS_DIR: &str = "builds";
pub(crate) const OBJECT_RECORDS_DIR: &str = "object-records";
pub(crate) const REUSES_DIR: &str = "reuses";
pub(crate) const OBJECT_REFS_DIR: &str = "object-refs";
pub(crate) const OBJECT_RECORD_REFS_DIR: &str = "object-record-refs";
pub(crate) const FS_FILES_DIR: &str = "fs-files";
pub(crate) const FS_TREES_DIR: &str = "fs-trees";
pub(crate) const QUARANTINE_DIR: &str = "quarantine";
pub(crate) const LOGS_DIR: &str = "logs";
pub(crate) const TMP_DIR: &str = "tmp";

/// Store-owned run log locations for the current store session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRunLogLocations {
    run_log_dir: PathBuf,
    run_id: String,
}

impl StoreRunLogLocations {
    /// Returns the run-level log directory.
    pub fn run_log_dir(&self) -> &Path {
        &self.run_log_dir
    }

    /// Returns the store run/session id.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

/// Store-owned paths allocated for one run subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreWorkspace {
    log_dir: PathBuf,
    raw_log_dir: PathBuf,
    temp_dir: PathBuf,
}

impl StoreWorkspace {
    fn new(log_dir: PathBuf, raw_log_dir: PathBuf, temp_dir: PathBuf) -> Self {
        Self {
            log_dir,
            raw_log_dir,
            temp_dir,
        }
    }

    /// Returns the per-subject log directory.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Returns the per-subject raw log directory.
    pub fn raw_log_dir(&self) -> &Path {
        &self.raw_log_dir
    }

    /// Returns the per-subject temporary directory.
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }
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

/// Immutable handle to a `bobr` store.
///
/// `Store` is the primary public interface for paths and operations that belong
/// to a store. It also represents one runtime session: creating a `Store`
/// allocates matching unique run directories under `<store>/logs` and
/// `<store>/tmp`.
///
/// A `Store` is cloneable and thread-safe. Cloning shares the same run log
/// directory, run temporary directory, and serial counter.
#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
}

#[derive(Debug)]
struct StoreInner {
    root: PathBuf,
    run_id: String,
    next_serial: AtomicU64,
    // Serializes appends to logs/<run-id>/index.jsonl when cloned Store
    // handles allocate workspaces concurrently.
    workspace_index_lock: Mutex<()>,
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
        ensure_store_layout(root)?;
        let run_id = allocate_store_run_id(root)?;
        Ok(Self {
            inner: Arc::new(StoreInner {
                root: root.to_path_buf(),
                run_id,
                next_serial: AtomicU64::new(0),
                workspace_index_lock: Mutex::new(()),
            }),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.inner.root
    }

    pub(crate) fn run_log_dir(&self) -> PathBuf {
        self.inner.root.join(LOGS_DIR).join(&self.inner.run_id)
    }

    pub(crate) fn run_tmp_dir(&self) -> PathBuf {
        self.inner.root.join(TMP_DIR).join(&self.inner.run_id)
    }

    /// Returns the run/session id of this store handle.
    pub fn run_id(&self) -> &str {
        &self.inner.run_id
    }

    /// Returns the run log locations allocated for this store session.
    pub fn run_log_locations(&self) -> StoreRunLogLocations {
        StoreRunLogLocations {
            run_log_dir: self.run_log_dir(),
            run_id: self.inner.run_id.clone(),
        }
    }

    /// Returns store-scoped fs-tree operations.
    pub fn fs_tree(&self) -> FsTree {
        FsTree::new(self.root().to_path_buf())
    }

    /// Returns the content-addressed legacy object directory.
    pub(crate) fn objects_dir(&self) -> PathBuf {
        self.root().join(OBJECTS_DIR)
    }

    /// Returns the build-key reference directory.
    pub(crate) fn builds_dir(&self) -> PathBuf {
        self.root().join(BUILDS_DIR)
    }

    /// Returns the reuse-key reference directory.
    pub(crate) fn reuses_dir(&self) -> PathBuf {
        self.root().join(REUSES_DIR)
    }

    /// Returns the JSON object record directory.
    pub(crate) fn object_records_dir(&self) -> PathBuf {
        self.root().join(OBJECT_RECORDS_DIR)
    }

    /// Returns the public object reference directory.
    pub(crate) fn object_refs_dir(&self) -> PathBuf {
        self.root().join(OBJECT_REFS_DIR)
    }

    /// Returns the public object-record reference directory.
    pub(crate) fn object_record_refs_dir(&self) -> PathBuf {
        self.root().join(OBJECT_RECORD_REFS_DIR)
    }

    fn quarantine_dir(&self) -> PathBuf {
        self.root().join(QUARANTINE_DIR)
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

    /// Returns the path of the JSON object record for `object_hash`.
    ///
    /// The path is under the JSON object record directory and has a `.json`
    /// suffix. The function does not check whether the record currently exists.
    pub(crate) fn object_record_path(&self, object_hash: ObjectHash) -> PathBuf {
        self.object_records_dir()
            .join(format!("{}.json", object_hash.to_hex()))
    }
}

/// Allocates store-owned paths for one concrete builder/source/scheduler run.
pub fn create_workspace(
    store: &Store,
    tag: impl Into<String>,
    recipe_name: Option<String>,
    build_key: impl Into<String>,
) -> Result<StoreWorkspace, StoreError> {
    let tag = tag.into();
    let build_key = build_key.into();
    let serial = store.inner.next_serial.fetch_add(1, Ordering::SeqCst);
    let directory_name = workspace_directory_name(serial, &tag, recipe_name.as_deref());
    let log_dir = store.run_log_dir().join(&directory_name);
    let temp_dir = store.run_tmp_dir().join(&directory_name);
    fs::create_dir(&log_dir).map_err(|error| {
        StoreError::Io(format!(
            "failed to create workspace log directory '{}': {error}",
            log_dir.display()
        ))
    })?;
    let raw_log_dir = log_dir.join("raw");
    fs::create_dir(&raw_log_dir).map_err(|error| {
        StoreError::Io(format!(
            "failed to create workspace raw log directory '{}': {error}",
            raw_log_dir.display()
        ))
    })?;
    fs::create_dir(&temp_dir).map_err(|error| {
        StoreError::Io(format!(
            "failed to create workspace temp directory '{}': {error}",
            temp_dir.display()
        ))
    })?;

    let record = WorkspaceLogRecord {
        serial,
        tag: &tag,
        recipe_name: recipe_name.as_deref(),
        build_key: &build_key,
        log_dir: &log_dir,
        raw_log_dir: &raw_log_dir,
        temp_dir: &temp_dir,
    };
    write_workspace_metadata(store, &record)?;
    append_workspace_index(store, &record)?;

    Ok(StoreWorkspace::new(log_dir, raw_log_dir, temp_dir))
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

fn ensure_store_layout(root: &Path) -> Result<(), StoreError> {
    ensure_store_dir(&root.join(OBJECTS_DIR), "objects")?;
    ensure_store_dir(&root.join(BUILDS_DIR), "builds")?;
    ensure_store_dir(&root.join(REUSES_DIR), "reuses")?;
    ensure_store_dir(&root.join(OBJECT_RECORDS_DIR), "object-records")?;
    ensure_store_dir(&root.join(OBJECT_REFS_DIR), "object-refs")?;
    ensure_store_dir(&root.join(OBJECT_RECORD_REFS_DIR), "object-record-refs")?;
    ensure_store_dir(&root.join(FS_FILES_DIR), "fs-files")?;
    ensure_store_dir(&root.join(FS_TREES_DIR), "fs-trees")?;
    ensure_store_dir(&root.join(LOGS_DIR), "logs")?;
    ensure_store_dir(&root.join(TMP_DIR), "tmp")?;
    Ok(())
}

pub(crate) fn allocate_store_run_id(root: &Path) -> Result<String, StoreError> {
    let now = OffsetDateTime::now_utc();
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local = now.to_offset(offset);
    let run_id_format =
        format_description!("[year repr:last_two][month][day][hour][minute][second]");
    let run_id_base = local
        .format(&run_id_format)
        .unwrap_or_else(|_| "000000000000".to_string());
    let logs_dir = root.join(LOGS_DIR);
    let tmp_dir = root.join(TMP_DIR);
    for attempt in 0..1000 {
        let run_id = if attempt == 0 {
            run_id_base.to_string()
        } else {
            format!("{run_id_base}.{attempt}")
        };
        let log_path = logs_dir.join(&run_id);
        let tmp_path = tmp_dir.join(&run_id);
        match fs::create_dir(&log_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(StoreError::Io(format!(
                    "failed to create run log directory '{}': {error}",
                    log_path.display()
                )));
            }
        }

        if let Err(error) = fs::create_dir(&tmp_path) {
            fs::remove_dir(&log_path).map_err(|cleanup_error| {
                StoreError::Io(format!(
                    "failed to remove unused run log directory '{}' after temp directory allocation failed at '{}': {cleanup_error}",
                    log_path.display(),
                    tmp_path.display()
                ))
            })?;
            return Err(StoreError::Io(format!(
                "failed to create run temp directory '{}': {error}",
                tmp_path.display()
            )));
        }
        return Ok(run_id);
    }

    Err(StoreError::Io(format!(
        "failed to allocate unique store run id for '{run_id_base}'"
    )))
}

fn workspace_directory_name(serial: u64, tag: &str, recipe_name: Option<&str>) -> String {
    let mut name = format!("{serial:08}-{}", safe_log_component_or(tag, "Builder"));
    if let Some(recipe_name) = recipe_name {
        let recipe_name = safe_log_component(recipe_name);
        if !recipe_name.is_empty() {
            name.push('-');
            name.push_str(&recipe_name);
        }
    }
    name
}

fn safe_log_component_or(value: &str, fallback: &str) -> String {
    let component = safe_log_component(value);
    if component.is_empty() {
        fallback.to_string()
    } else {
        component
    }
}

fn safe_log_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '_',
        })
        .collect()
}

struct WorkspaceLogRecord<'a> {
    serial: u64,
    tag: &'a str,
    recipe_name: Option<&'a str>,
    build_key: &'a str,
    log_dir: &'a Path,
    raw_log_dir: &'a Path,
    temp_dir: &'a Path,
}

fn write_workspace_metadata(
    store: &Store,
    record: &WorkspaceLogRecord<'_>,
) -> Result<(), StoreError> {
    let mut metadata = Map::new();
    metadata.insert(
        "schema".to_string(),
        Value::String("bobr-workspace-v1".to_string()),
    );
    metadata.insert("serial".to_string(), Value::Number(record.serial.into()));
    metadata.insert("tag".to_string(), Value::String(record.tag.to_string()));
    if let Some(recipe_name) = record.recipe_name {
        metadata.insert(
            "recipe_name".to_string(),
            Value::String(recipe_name.to_string()),
        );
    }
    metadata.insert(
        "build_key".to_string(),
        Value::String(record.build_key.to_string()),
    );
    metadata.insert(
        "run_id".to_string(),
        Value::String(store.run_id().to_string()),
    );
    metadata.insert(
        "log_dir".to_string(),
        Value::String(record.log_dir.display().to_string()),
    );
    metadata.insert(
        "raw_log_dir".to_string(),
        Value::String(record.raw_log_dir.display().to_string()),
    );
    metadata.insert(
        "temp_dir".to_string(),
        Value::String(record.temp_dir.display().to_string()),
    );
    let path = record.log_dir.join("meta.json");
    let bytes = serde_json::to_vec_pretty(&Value::Object(metadata)).map_err(|error| {
        StoreError::InvalidData(format!("failed to encode workspace metadata: {error}"))
    })?;
    fs::write(&path, bytes).map_err(|error| {
        StoreError::Io(format!(
            "failed to write workspace metadata '{}': {error}",
            path.display()
        ))
    })
}

fn append_workspace_index(
    store: &Store,
    record: &WorkspaceLogRecord<'_>,
) -> Result<(), StoreError> {
    let _guard = store
        .inner
        .workspace_index_lock
        .lock()
        .map_err(|error| StoreError::Io(format!("failed to lock workspace index: {error}")))?;
    let path = store.run_log_dir().join("index.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| {
            StoreError::Io(format!(
                "failed to open workspace index '{}': {error}",
                path.display()
            ))
        })?;
    let record = json!({
        "serial": record.serial,
        "tag": record.tag,
        "recipe_name": record.recipe_name,
        "build_key": record.build_key,
        "log_dir": record.log_dir.display().to_string(),
    });
    let line = serde_json::to_string(&record).map_err(|error| {
        StoreError::InvalidData(format!("failed to encode workspace index record: {error}"))
    })?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| {
            StoreError::Io(format!(
                "failed to append workspace index '{}': {error}",
                path.display()
            ))
        })
}

/// Removes and recreates a store-owned temporary directory.
///
/// The directory must be below the store temporary root. This guard keeps
/// force-removal scoped to temporary directories that belong to the store.
/// The resulting directory exists and is empty.
pub fn recreate_store_temp_dir_force(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    validate_store_temp_dir(store, temp_dir)?;
    private_fs::recreate_empty_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

/// Removes a store-owned temporary directory if it exists.
///
/// The directory must be below the store temporary root. Missing directories are
/// treated as success.
pub fn remove_store_temp_dir_force(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    validate_store_temp_dir(store, temp_dir)?;
    private_fs::remove_dir_force(temp_dir).map_err(crate::error::map_fsutil_error)
}

fn validate_store_temp_dir(store: &Store, temp_dir: &Path) -> Result<(), StoreError> {
    if temp_dir
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(StoreError::InvalidInput(format!(
            "store temp directory '{}' must not contain '..' path components",
            temp_dir.display()
        )));
    }

    let store_tmp_dir = store.root().join(TMP_DIR);
    if temp_dir == store_tmp_dir || !temp_dir.starts_with(&store_tmp_dir) {
        return Err(StoreError::InvalidInput(format!(
            "store temp directory '{}' must be under store temp root '{}'",
            temp_dir.display(),
            store_tmp_dir.display()
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
        "schema": "bobr-quarantine-v1",
        "builder_tag": &request.builder_tag,
        "build_key": request.build_key.to_hex(),
        "original_path": request.temp_path.display().to_string(),
        "quarantine_path": target.display().to_string(),
        "reason": &request.reason,
        "quarantined_at_unix_nanos": stamp.to_string(),
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
