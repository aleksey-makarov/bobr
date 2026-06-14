use crate::ObjectHash;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use time::UtcOffset;
use time::macros::format_description;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildLogLevel {
    Info,
    Warn,
    Error,
}

impl fmt::Display for BuildLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warn => f.write_str("warn"),
            Self::Error => f.write_str("error"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildLogEvent {
    pub level: BuildLogLevel,
    pub phase: String,
    pub message: String,
    pub object_hash: Option<ObjectHash>,
    pub raw_log_path: Option<PathBuf>,
    pub details: Map<String, Value>,
}

pub trait BuildLogger: fmt::Debug + Send + Sync {
    fn log_event(&self, event: BuildLogEvent);

    fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, String>;
}

#[derive(Debug, Default)]
pub struct NoopBuildLogger;

impl BuildLogger for NoopBuildLogger {
    fn log_event(&self, _event: BuildLogEvent) {}

    fn allocate_raw_log_path(&self, _label: &str) -> Result<PathBuf, String> {
        Err("no build logger configured".to_string())
    }
}

#[derive(Debug)]
pub struct BuildRunLogger {
    run_log_dir: PathBuf,
    event_log_path: PathBuf,
    emit_progress: bool,
    run_id: String,
    writer: Mutex<BufWriter<File>>,
}

impl BuildRunLogger {
    pub fn new(run_log_dir: &Path, run_id: &str, emit_progress: bool) -> Result<Self, String> {
        fs::create_dir_all(run_log_dir).map_err(|error| {
            format!(
                "failed to create run log directory '{}': {error}",
                run_log_dir.display()
            )
        })?;

        let event_log_path = run_log_dir.join("events.jsonl");
        let file = create_event_log_file(&event_log_path).map_err(|error| {
            format!(
                "failed to create run event log '{}': {error}",
                event_log_path.display()
            )
        })?;

        Ok(Self {
            run_log_dir: run_log_dir.to_path_buf(),
            event_log_path,
            emit_progress,
            run_id: run_id.to_string(),
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn run_log_dir(&self) -> &Path {
        &self.run_log_dir
    }

    pub fn bind_subject(
        self: &Arc<Self>,
        subject: BuildLogSubject,
    ) -> Result<Arc<dyn BuildLogger>, String> {
        fs::create_dir_all(&subject.log_dir).map_err(|error| {
            format!(
                "failed to create subject log directory '{}': {error}",
                subject.log_dir.display()
            )
        })?;
        fs::create_dir_all(&subject.raw_log_dir).map_err(|error| {
            format!(
                "failed to create raw log directory '{}': {error}",
                subject.raw_log_dir.display()
            )
        })?;
        let event_log_path = subject.log_dir.join("events.jsonl");
        let event_file = create_event_log_file(&event_log_path).map_err(|error| {
            format!(
                "failed to create subject event log '{}': {error}",
                event_log_path.display()
            )
        })?;
        Ok(Arc::new(BoundBuildLogger {
            inner: self.clone(),
            subject,
            event_log_path,
            writer: Mutex::new(BufWriter::new(event_file)),
            raw_log_counters: Mutex::new(BTreeMap::new()),
        }))
    }

    fn write_event(
        &self,
        builder: &str,
        name: &str,
        build_key: &str,
        event: &BuildLogEvent,
    ) -> Result<(), String> {
        let mut writer = self.writer.lock().map_err(|error| error.to_string())?;
        let line =
            serde_json::to_string(&EventLogRecord::from_event(builder, name, build_key, event))
                .map_err(|error| format!("failed to serialize build event: {error}"))?;
        writer
            .write_all(line.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .and_then(|_| writer.flush())
            .map_err(|error| {
                format!(
                    "failed to append event log '{}': {error}",
                    self.event_log_path.display()
                )
            })
    }

    fn log_bound_event(&self, subject: &BuildLogSubject, event: &BuildLogEvent) {
        if self.emit_progress {
            eprintln!(
                "{}",
                format_progress_line(&subject.tag, &subject.name, &subject.build_key, event)
            );
        }

        if let Err(error) = self.write_event(&subject.tag, &subject.name, &subject.build_key, event)
        {
            eprintln!("warning: {error}");
        }
    }
}

/// Log identity and paths for one concrete builder or source run.
#[derive(Debug, Clone)]
pub struct BuildLogSubject {
    tag: String,
    name: String,
    build_key: String,
    log_dir: PathBuf,
    raw_log_dir: PathBuf,
}

impl BuildLogSubject {
    /// Creates a log subject from runtime-allocated identity and log paths.
    pub fn new(
        tag: impl Into<String>,
        name: impl Into<String>,
        build_key: impl Into<String>,
        log_dir: PathBuf,
        raw_log_dir: PathBuf,
    ) -> Self {
        Self {
            tag: tag.into(),
            name: name.into(),
            build_key: build_key.into(),
            log_dir,
            raw_log_dir,
        }
    }
}

#[derive(Debug)]
struct BoundBuildLogger {
    inner: Arc<BuildRunLogger>,
    subject: BuildLogSubject,
    event_log_path: PathBuf,
    writer: Mutex<BufWriter<File>>,
    raw_log_counters: Mutex<BTreeMap<String, usize>>,
}

impl BuildLogger for BoundBuildLogger {
    fn log_event(&self, event: BuildLogEvent) {
        self.inner.log_bound_event(&self.subject, &event);
        if let Err(error) = self.write_event(&event) {
            eprintln!("warning: {error}");
        }
    }

    fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, String> {
        fs::create_dir_all(&self.subject.raw_log_dir).map_err(|error| {
            format!(
                "failed to create raw log directory '{}': {error}",
                self.subject.raw_log_dir.display()
            )
        })?;
        let base = sanitize_component(label);
        unique_path(
            &self.subject.raw_log_dir,
            &base,
            "log",
            &self.raw_log_counters,
        )
    }
}

impl BoundBuildLogger {
    fn write_event(&self, event: &BuildLogEvent) -> Result<(), String> {
        let mut writer = self.writer.lock().map_err(|error| error.to_string())?;
        let line = serde_json::to_string(&EventLogRecord::from_event(
            &self.subject.tag,
            &self.subject.name,
            &self.subject.build_key,
            event,
        ))
        .map_err(|error| format!("failed to serialize build event: {error}"))?;
        writer
            .write_all(line.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .and_then(|_| writer.flush())
            .map_err(|error| {
                format!(
                    "failed to append subject event log '{}': {error}",
                    self.event_log_path.display()
                )
            })
    }
}

#[derive(Debug, Serialize)]
struct EventLogRecord {
    ts: String,
    level: String,
    phase: String,
    builder: String,
    name: String,
    build_key: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    object_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    details: Map<String, Value>,
}

impl EventLogRecord {
    fn from_event(builder: &str, name: &str, build_key: &str, event: &BuildLogEvent) -> Self {
        let mut details = event.details.clone();
        details.insert(
            "full_build_key".to_string(),
            Value::String(build_key.to_string()),
        );
        if let Some(object_hash) = event.object_hash {
            details.insert(
                "full_object_hash".to_string(),
                Value::String(object_hash.to_string()),
            );
        }

        Self {
            ts: current_human_timestamp(),
            level: event.level.to_string(),
            phase: event.phase.clone(),
            builder: builder.to_string(),
            name: name.to_string(),
            build_key: short_build_key(build_key),
            message: event.message.clone(),
            object_hash: event.object_hash.map(short_object_hash),
            raw_log_path: event
                .raw_log_path
                .as_ref()
                .map(|path| path.display().to_string()),
            details,
        }
    }
}

fn format_progress_line(
    builder: &str,
    name: &str,
    build_key: &str,
    event: &BuildLogEvent,
) -> String {
    let mut line = format!(
        "[{}] {} {} {}",
        event.phase,
        builder,
        name,
        short_build_key(build_key)
    );

    if let Some(object_hash) = event.object_hash {
        line.push(' ');
        line.push_str(&short_object_hash(object_hash));
    }

    if !event.message.is_empty() {
        line.push_str(": ");
        line.push_str(&event.message);
    }

    if let Some(path) = &event.raw_log_path {
        line.push_str(" (log: ");
        line.push_str(&path.display().to_string());
        line.push(')');
    }

    line
}

fn short_build_key(build_key: &str) -> String {
    build_key.chars().take(12).collect()
}

fn short_object_hash(object_hash: ObjectHash) -> String {
    object_hash.to_string().chars().take(12).collect()
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '_',
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

fn current_timestamp_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn current_human_timestamp() -> String {
    let now = current_timestamp_utc();
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local = now.to_offset(offset);
    let format = format_description!("[year repr:last_two][month][day][hour][minute][second]");
    local
        .format(&format)
        .unwrap_or_else(|_| "000000000000".to_string())
}

fn unique_path(
    dir: &Path,
    base: &str,
    extension: &str,
    counters: &Mutex<BTreeMap<String, usize>>,
) -> Result<PathBuf, String> {
    let key = format!("{}/{}.{}", dir.display(), base, extension);
    let mut counters = counters.lock().map_err(|error| error.to_string())?;
    let counter = counters.entry(key).or_insert(0);
    *counter += 1;
    let suffix = if *counter == 1 {
        String::new()
    } else {
        format!("-{}", *counter)
    };
    Ok(dir.join(format!("{base}{suffix}.{extension}")))
}

fn create_event_log_file(path: &Path) -> Result<File, String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn bound_logger_writes_builder_identity_to_event_log() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", false).unwrap(),
        );
        let build_key = "1111111111111111111111111111111111111111111111111111111111111111";
        let subject_dir = run_log_dir.join("00000000-Sandbox-bash");
        let subject = BuildLogSubject::new(
            "Sandbox",
            "bash",
            build_key,
            subject_dir.clone(),
            subject_dir.join("raw"),
        );
        let node_logger = logger.bind_subject(subject).unwrap();

        node_logger.log_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            phase: "start".to_string(),
            message: "starting builder node".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });

        let contents = fs::read_to_string(&logger.event_log_path).unwrap();
        let line = contents.lines().last().unwrap();
        let event: Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["builder"], Value::String("Sandbox".to_string()));
        assert_eq!(event["name"], Value::String("bash".to_string()));
        assert_eq!(
            event["build_key"],
            Value::String(short_build_key(build_key))
        );
        assert_eq!(
            event["details"]["full_build_key"],
            Value::String(build_key.to_string())
        );

        let subject_contents = fs::read_to_string(subject_dir.join("events.jsonl")).unwrap();
        let subject_line = subject_contents.lines().last().unwrap();
        let subject_event: Value = serde_json::from_str(subject_line).unwrap();
        assert_eq!(
            subject_event["builder"],
            Value::String("Sandbox".to_string())
        );
    }

    #[test]
    fn bound_logger_allocates_raw_logs_under_subject_raw_dir() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", false).unwrap(),
        );
        let build_key = "2222222222222222222222222222222222222222222222222222222222222222";
        let subject_dir = run_log_dir.join("00000000-Sandbox-bash_debug_test");
        let raw_dir = subject_dir.join("raw");
        let subject = BuildLogSubject::new(
            "Sandbox",
            "bash debug/test",
            build_key,
            subject_dir,
            raw_dir.clone(),
        );
        let node_logger = logger.bind_subject(subject).unwrap();

        let path = node_logger.allocate_raw_log_path("podman/run").unwrap();
        assert!(path.starts_with(&raw_dir));
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("log")
        );
        assert!(
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap()
                .contains("podman_run")
        );
    }
}
