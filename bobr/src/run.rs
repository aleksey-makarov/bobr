//! The current build run: its identity, its two directories, and the
//! per-subject workspaces allocated in them.
//!
//! A run is the execution-scoped counterpart of the store. The store is
//! content-addressed and outlives any single build; a run lasts for one
//! invocation and owns everything named after it — the log directory, the work
//! directory, the serial numbers of the workspaces handed out, and the run id
//! stamped into what those workspaces record. Keeping this out of [`Store`] is
//! deliberate: the store answers questions about objects, not about where one
//! particular build writes its logs and scratch.
//!
//! [`Store`]: bobr_store::Store

use crate::execution::ExecutionError;
use bobr_core::Workspace;
use bobr_core::fsutil;
use serde_json::{Map, Value, json};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

const LOGS_DIR: &str = "logs";
const WORK_DIR: &str = "tmp";

/// One build run: where it writes and what it calls itself.
#[derive(Debug)]
pub(crate) struct Run {
    run_id: String,
    logs_dir: PathBuf,
    work_dir: PathBuf,
    /// Numbers workspaces in allocation order across the whole run, so a
    /// subject's directory name says when it ran.
    next_serial: AtomicU64,
    /// Serializes appends to `index.jsonl` while subjects run in parallel.
    index_lock: Mutex<()>,
}

impl Run {
    /// Allocates a run under a store root, in `<root>/logs/<run-id>` and
    /// `<root>/tmp/<run-id>`.
    ///
    /// The run id is the local timestamp, with `.1`, `.2`, … appended until a
    /// pair of directories is free. `create_dir` is what claims them: it fails
    /// rather than reuses, so two runs started in the same second cannot land
    /// on the same directories.
    pub(crate) fn allocate_in_store(store_root: &Path) -> Result<Self, ExecutionError> {
        let logs_root = store_root.join(LOGS_DIR);
        let work_root = store_root.join(WORK_DIR);
        for (root, label) in [(&logs_root, "log"), (&work_root, "work")] {
            fs::create_dir_all(root).map_err(|error| {
                ExecutionError::Run(format!(
                    "failed to create run {label} root '{}': {error}",
                    root.display()
                ))
            })?;
        }

        let now = OffsetDateTime::now_utc();
        let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let local = now.to_offset(offset);
        let run_id_format =
            format_description!("[year repr:last_two][month][day][hour][minute][second]");
        let run_id_base = local
            .format(&run_id_format)
            .unwrap_or_else(|_| "000000000000".to_string());

        for attempt in 0..1000 {
            let run_id = if attempt == 0 {
                run_id_base.to_string()
            } else {
                format!("{run_id_base}.{attempt}")
            };
            let logs_dir = logs_root.join(&run_id);
            let work_dir = work_root.join(&run_id);
            match fs::create_dir(&logs_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(ExecutionError::Run(format!(
                        "failed to create run log directory '{}': {error}",
                        logs_dir.display()
                    )));
                }
            }

            if let Err(error) = fs::create_dir(&work_dir) {
                fs::remove_dir(&logs_dir).map_err(|cleanup_error| {
                    ExecutionError::Run(format!(
                        "failed to remove unused run log directory '{}' after work directory allocation failed at '{}': {cleanup_error}",
                        logs_dir.display(),
                        work_dir.display()
                    ))
                })?;
                return Err(ExecutionError::Run(format!(
                    "failed to create run work directory '{}': {error}",
                    work_dir.display()
                )));
            }

            return Ok(Self::new(run_id, logs_dir, work_dir));
        }

        Err(ExecutionError::Run(format!(
            "failed to allocate a unique run id for '{run_id_base}'"
        )))
    }

    fn new(run_id: String, logs_dir: PathBuf, work_dir: PathBuf) -> Self {
        Self {
            run_id,
            logs_dir,
            work_dir,
            next_serial: AtomicU64::new(0),
            index_lock: Mutex::new(()),
        }
    }

    /// Returns the id this run is recorded under.
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Returns the run-level log directory.
    pub(crate) fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }

    /// Returns the run-level work directory.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Allocates the directories for one subject: its log directory, the `raw`
    /// subdirectory for captured tool output, and its scratch directory.
    ///
    /// Both directories are created exclusively, so a run pointed at
    /// directories another run already used fails here rather than merging into
    /// them. The allocation is also recorded twice: in the subject's own
    /// `meta.json`, and as a line in the run's `index.jsonl`.
    pub(crate) fn create_workspace(
        &self,
        tag: impl Into<String>,
        name: impl Into<String>,
        build_key: impl Into<String>,
    ) -> Result<Workspace, ExecutionError> {
        let tag = tag.into();
        let name = name.into();
        let build_key = build_key.into();
        let serial = self.next_serial.fetch_add(1, Ordering::SeqCst);
        let directory_name = workspace_directory_name(serial, &tag, &name);
        let log_dir = self.logs_dir.join(&directory_name);
        let temp_dir = self.work_dir.join(&directory_name);
        fs::create_dir(&log_dir).map_err(|error| {
            ExecutionError::Run(format!(
                "failed to create workspace log directory '{}': {error}",
                log_dir.display()
            ))
        })?;
        let raw_log_dir = log_dir.join("raw");
        fs::create_dir(&raw_log_dir).map_err(|error| {
            ExecutionError::Run(format!(
                "failed to create workspace raw log directory '{}': {error}",
                raw_log_dir.display()
            ))
        })?;
        fs::create_dir(&temp_dir).map_err(|error| {
            ExecutionError::Run(format!(
                "failed to create workspace work directory '{}': {error}",
                temp_dir.display()
            ))
        })?;

        let record = WorkspaceLogRecord {
            serial,
            tag: &tag,
            name: &name,
            build_key: &build_key,
            log_dir: &log_dir,
            raw_log_dir: &raw_log_dir,
            temp_dir: &temp_dir,
        };
        self.write_workspace_metadata(&record)?;
        self.append_workspace_index(&record)?;

        Ok(Workspace::new(log_dir, raw_log_dir, temp_dir))
    }

    /// Empties a subject's scratch directory before its builder runs.
    pub(crate) fn prepare_scratch(&self, scratch_dir: &Path) -> Result<(), ExecutionError> {
        self.validate_scratch(scratch_dir)?;
        fsutil::recreate_empty_dir_force(scratch_dir).map_err(|error| {
            ExecutionError::Run(format!(
                "failed to prepare scratch directory '{}': {error}",
                scratch_dir.display()
            ))
        })
    }

    /// Removes a subject's scratch directory once its builder is done.
    pub(crate) fn remove_scratch(&self, scratch_dir: &Path) -> Result<(), ExecutionError> {
        self.validate_scratch(scratch_dir)?;
        fsutil::remove_dir_force(scratch_dir).map_err(|error| {
            ExecutionError::Run(format!(
                "failed to remove scratch directory '{}': {error}",
                scratch_dir.display()
            ))
        })
    }

    /// Guards the force-removals above: they may only touch paths inside this
    /// run's work directory.
    fn validate_scratch(&self, scratch_dir: &Path) -> Result<(), ExecutionError> {
        if scratch_dir
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(ExecutionError::Run(format!(
                "scratch directory '{}' must not contain '..' path components",
                scratch_dir.display()
            )));
        }

        if scratch_dir == self.work_dir || !scratch_dir.starts_with(&self.work_dir) {
            return Err(ExecutionError::Run(format!(
                "scratch directory '{}' must be under the run work directory '{}'",
                scratch_dir.display(),
                self.work_dir.display()
            )));
        }

        Ok(())
    }

    fn write_workspace_metadata(
        &self,
        record: &WorkspaceLogRecord<'_>,
    ) -> Result<(), ExecutionError> {
        let mut metadata = Map::new();
        metadata.insert(
            "schema".to_string(),
            Value::String("bobr-workspace-v2".to_string()),
        );
        metadata.insert("serial".to_string(), Value::Number(record.serial.into()));
        metadata.insert("tag".to_string(), Value::String(record.tag.to_string()));
        metadata.insert("name".to_string(), Value::String(record.name.to_string()));
        metadata.insert(
            "build_key".to_string(),
            Value::String(record.build_key.to_string()),
        );
        metadata.insert("run_id".to_string(), Value::String(self.run_id.clone()));
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
            ExecutionError::Run(format!("failed to encode workspace metadata: {error}"))
        })?;
        fs::write(&path, bytes).map_err(|error| {
            ExecutionError::Run(format!(
                "failed to write workspace metadata '{}': {error}",
                path.display()
            ))
        })
    }

    fn append_workspace_index(
        &self,
        record: &WorkspaceLogRecord<'_>,
    ) -> Result<(), ExecutionError> {
        let _guard = self.index_lock.lock().map_err(|error| {
            ExecutionError::Run(format!("failed to lock the workspace index: {error}"))
        })?;
        let path = self.logs_dir.join("index.jsonl");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                ExecutionError::Run(format!(
                    "failed to open workspace index '{}': {error}",
                    path.display()
                ))
            })?;
        let record = json!({
            "serial": record.serial,
            "tag": record.tag,
            "name": record.name,
            "build_key": record.build_key,
            "log_dir": record.log_dir.display().to_string(),
        });
        let line = serde_json::to_string(&record).map_err(|error| {
            ExecutionError::Run(format!(
                "failed to encode the workspace index record: {error}"
            ))
        })?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|error| {
                ExecutionError::Run(format!(
                    "failed to append to the workspace index '{}': {error}",
                    path.display()
                ))
            })
    }
}

struct WorkspaceLogRecord<'a> {
    serial: u64,
    tag: &'a str,
    name: &'a str,
    build_key: &'a str,
    log_dir: &'a Path,
    raw_log_dir: &'a Path,
    temp_dir: &'a Path,
}

fn workspace_directory_name(serial: u64, tag: &str, name: &str) -> String {
    let mut directory = format!("{serial:08}-{}", safe_log_component_or(tag, "Builder"));
    let name = safe_log_component(name);
    if !name.is_empty() {
        directory.push('-');
        directory.push_str(&name);
    }
    directory
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn allocates_matching_log_and_work_directories() {
        let temp = tempdir().unwrap();

        let first = Run::allocate_in_store(temp.path()).unwrap();
        let second = Run::allocate_in_store(temp.path()).unwrap();

        assert_ne!(first.logs_dir(), second.logs_dir());
        assert_ne!(first.work_dir(), second.work_dir());
        for run in [&first, &second] {
            assert!(run.logs_dir().is_dir());
            assert!(run.work_dir().is_dir());
            // One run id names both directories.
            assert_eq!(
                run.logs_dir().file_name().unwrap(),
                run.work_dir().file_name().unwrap()
            );
            assert_eq!(run.logs_dir().file_name().unwrap(), run.run_id());
        }
    }

    #[test]
    fn allocation_disambiguates_by_log_directory() {
        for _ in 0..100 {
            let temp = tempdir().unwrap();
            let logs_root = temp.path().join(LOGS_DIR);
            let work_root = temp.path().join(WORK_DIR);

            let first = Run::allocate_in_store(temp.path()).unwrap();
            let first_suffix = format!("{}.1", first.run_id());
            let second_suffix = format!("{}.2", first.run_id());
            fs::create_dir(logs_root.join(&first_suffix)).unwrap();

            let run = Run::allocate_in_store(temp.path()).unwrap();
            if run.run_id() != second_suffix {
                // The second allocation landed in a later timestamp second.
                assert!(
                    !run.run_id().starts_with(&format!("{}.", first.run_id())),
                    "unexpected run id suffix after log directory collision: {}",
                    run.run_id()
                );
                continue;
            }

            assert!(run.logs_dir().is_dir());
            assert!(run.work_dir().is_dir());
            assert!(logs_root.join(&first_suffix).is_dir());
            assert!(!work_root.join(&first_suffix).exists());
            return;
        }

        panic!("could not perform two run id allocations inside one timestamp second");
    }

    #[test]
    fn allocation_fails_when_only_the_work_directory_is_taken() {
        for _ in 0..100 {
            let temp = tempdir().unwrap();
            let logs_root = temp.path().join(LOGS_DIR);
            let work_root = temp.path().join(WORK_DIR);

            let first = Run::allocate_in_store(temp.path()).unwrap();
            let conflicting = format!("{}.1", first.run_id());
            fs::create_dir(work_root.join(&conflicting)).unwrap();

            match Run::allocate_in_store(temp.path()) {
                Ok(run) => {
                    assert!(
                        !run.run_id().starts_with(&format!("{}.", first.run_id())),
                        "allocator ignored a taken work directory: {}",
                        run.run_id()
                    );
                    continue;
                }
                Err(error) => {
                    assert!(matches!(
                        error,
                        ExecutionError::Run(message)
                            if message.contains("failed to create run work directory")
                    ));
                    // The log directory claimed for the failed attempt is given back.
                    assert!(!logs_root.join(&conflicting).exists());
                    assert!(!logs_root.join(format!("{}.2", first.run_id())).exists());
                    assert!(work_root.join(&conflicting).is_dir());
                    return;
                }
            }
        }

        panic!("could not test a taken work directory inside one timestamp second");
    }

    #[test]
    fn workspace_allocation_writes_metadata_index_and_sanitized_paths() {
        let temp = tempdir().unwrap();
        let run = Run::allocate_in_store(temp.path()).unwrap();

        let workspace = run
            .create_workspace(
                "Source Builder",
                "name / demo",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .unwrap();

        assert_eq!(
            workspace.log_dir().file_name().unwrap().to_str().unwrap(),
            "00000000-Source_Builder-name___demo"
        );
        assert!(workspace.raw_log_dir().is_dir());
        assert!(workspace.temp_dir().is_dir());
        assert!(workspace.log_dir().starts_with(run.logs_dir()));
        assert!(workspace.raw_log_dir().starts_with(workspace.log_dir()));
        assert!(workspace.temp_dir().starts_with(run.work_dir()));
        assert!(!workspace.temp_dir().starts_with(workspace.log_dir()));
        assert_eq!(
            workspace.log_dir().file_name().unwrap(),
            workspace.temp_dir().file_name().unwrap()
        );

        let metadata: Value =
            serde_json::from_slice(&fs::read(workspace.log_dir().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["schema"], "bobr-workspace-v2");
        assert_eq!(metadata["serial"], 0);
        assert_eq!(metadata["tag"], "Source Builder");
        assert_eq!(metadata["name"], "name / demo");
        assert_eq!(
            metadata["build_key"],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(metadata["run_id"], run.run_id());
        assert_eq!(
            metadata["temp_dir"],
            workspace.temp_dir().display().to_string()
        );

        let index = fs::read_to_string(run.logs_dir().join("index.jsonl")).unwrap();
        let records = index.lines().collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        let record: Value = serde_json::from_str(records[0]).unwrap();
        assert_eq!(record["serial"], 0);
        assert_eq!(record["tag"], "Source Builder");
        assert_eq!(record["name"], "name / demo");
    }

    #[test]
    fn workspace_serials_count_up_within_one_run() {
        let temp = tempdir().unwrap();
        let run = Run::allocate_in_store(temp.path()).unwrap();

        let first = run.create_workspace("Tree", "left", "build-left").unwrap();
        let second = run
            .create_workspace("Tree", "right", "build-right")
            .unwrap();

        assert_eq!(
            first.log_dir().file_name().unwrap().to_str().unwrap(),
            "00000000-Tree-left"
        );
        assert_eq!(
            second.log_dir().file_name().unwrap().to_str().unwrap(),
            "00000001-Tree-right"
        );
        assert_eq!(
            first.temp_dir().file_name().unwrap(),
            first.log_dir().file_name().unwrap()
        );
    }

    #[test]
    fn parallel_workspace_allocation_does_not_reuse_serials() {
        let temp = tempdir().unwrap();
        let run = Arc::new(Run::allocate_in_store(temp.path()).unwrap());
        let mut handles = Vec::new();

        for index in 0..8 {
            let run = run.clone();
            handles.push(thread::spawn(move || {
                run.create_workspace("Tree", format!("node-{index}"), format!("build-{index}"))
                    .unwrap()
                    .log_dir()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            }));
        }

        let names = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), 8);
        for serial in 0..8 {
            let prefix = format!("{serial:08}-Tree-node-");
            assert!(names.iter().any(|name| name.starts_with(&prefix)));
        }
    }

    #[test]
    fn scratch_is_prepared_empty_and_removed() {
        let temp = tempdir().unwrap();
        let run = Run::allocate_in_store(temp.path()).unwrap();
        let workspace = run.create_workspace("Tree", "demo", "build-demo").unwrap();
        let scratch = workspace.temp_dir().to_path_buf();
        fs::write(scratch.join("stale"), b"old\n").unwrap();

        run.prepare_scratch(&scratch).unwrap();

        assert!(scratch.is_dir());
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);

        run.remove_scratch(&scratch).unwrap();

        assert!(!scratch.exists());
        // Removing what is already gone is not an error.
        run.remove_scratch(&scratch).unwrap();
    }

    #[test]
    fn scratch_operations_refuse_paths_outside_the_run() {
        let temp = tempdir().unwrap();
        let run = Run::allocate_in_store(temp.path()).unwrap();
        let outside = temp.path().join("elsewhere");
        fs::create_dir(&outside).unwrap();

        for path in [outside.as_path(), run.work_dir()] {
            let error = run.remove_scratch(path).unwrap_err();
            assert!(matches!(
                error,
                ExecutionError::Run(message) if message.contains("must be under the run work directory")
            ));
        }
        assert!(outside.is_dir());

        let escaping = run.work_dir().join("..").join("escape");
        let error = run.prepare_scratch(&escaping).unwrap_err();
        assert!(matches!(
            error,
            ExecutionError::Run(message) if message.contains("'..' path components")
        ));
    }
}
