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

#[derive(Debug, Clone)]
pub struct RunTimestamp {
    human: String,
    rfc3339_utc: String,
}

impl RunTimestamp {
    pub fn now() -> Self {
        let now = current_timestamp_utc();
        let human_format =
            format_description!("[year repr:last_two][month][day][hour][minute][second]");
        let rfc3339_format = format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
        );
        let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let local = now.to_offset(offset);
        Self {
            human: local
                .format(&human_format)
                .unwrap_or_else(|_| "000000000000".to_string()),
            rfc3339_utc: now
                .format(&rfc3339_format)
                .unwrap_or_else(|_| "1970-01-01T00:00:00.000000000Z".to_string()),
        }
    }

    pub fn human(&self) -> &str {
        &self.human
    }

    pub fn rfc3339_utc(&self) -> &str {
        &self.rfc3339_utc
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunOptions {
    pub emit_progress: bool,
}

#[derive(Debug)]
pub struct BuildRunLogger {
    store_root: PathBuf,
    event_log_path: PathBuf,
    emit_progress: bool,
    run_timestamp: RunTimestamp,
    writer: Mutex<BufWriter<File>>,
    raw_log_counters: Mutex<BTreeMap<String, usize>>,
}

impl BuildRunLogger {
    pub fn new(store_root: &Path, options: RunOptions) -> Result<Self, String> {
        let runs_dir = store_root.join("logs").join("runs");
        fs::create_dir_all(&runs_dir).map_err(|error| {
            format!(
                "failed to create run logs directory '{}': {error}",
                runs_dir.display()
            )
        })?;

        let run_timestamp = RunTimestamp::now();
        let pid = std::process::id();
        let (event_log_path, file) =
            create_run_log_file(&runs_dir, &format!("{}-{pid}", run_timestamp.human())).map_err(
                |error| {
                    format!(
                        "failed to create event log under '{}': {error}",
                        runs_dir.display()
                    )
                },
            )?;

        Ok(Self {
            store_root: store_root.to_path_buf(),
            event_log_path,
            emit_progress: options.emit_progress,
            run_timestamp,
            writer: Mutex::new(BufWriter::new(file)),
            raw_log_counters: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn created_at(&self) -> &str {
        self.run_timestamp.rfc3339_utc()
    }

    pub fn bind_node(
        self: &Arc<Self>,
        builder: impl Into<String>,
        name: impl Into<String>,
        build_key: impl fmt::Display,
    ) -> Arc<dyn BuildLogger> {
        Arc::new(BoundBuildLogger {
            inner: self.clone(),
            builder: builder.into(),
            name: name.into(),
            build_key: build_key.to_string(),
        })
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

    fn log_bound_event(&self, builder: &str, name: &str, build_key: &str, event: &BuildLogEvent) {
        if self.emit_progress {
            eprintln!("{}", format_progress_line(builder, name, build_key, event));
        }

        if let Err(error) = self.write_event(builder, name, build_key, event) {
            eprintln!("warning: {error}");
        }
    }

    fn allocate_node_raw_log_path(
        &self,
        builder: &str,
        name: &str,
        build_key: &str,
        label: &str,
    ) -> Result<PathBuf, String> {
        let logs_dir = self
            .store_root
            .join("builder-state")
            .join(builder.to_ascii_lowercase())
            .join("logs")
            .join(sanitize_component(name));
        fs::create_dir_all(&logs_dir).map_err(|error| {
            format!(
                "failed to create raw logs directory '{}': {error}",
                logs_dir.display()
            )
        })?;

        let short_build_key = short_build_key(build_key);
        let base = format!(
            "{}-{}-{}",
            self.run_timestamp.human(),
            short_build_key,
            sanitize_component(label)
        );
        unique_path(&logs_dir, &base, "log", &self.raw_log_counters)
    }
}

#[derive(Debug)]
struct BoundBuildLogger {
    inner: Arc<BuildRunLogger>,
    builder: String,
    name: String,
    build_key: String,
}

impl BuildLogger for BoundBuildLogger {
    fn log_event(&self, event: BuildLogEvent) {
        self.inner
            .log_bound_event(&self.builder, &self.name, &self.build_key, &event);
    }

    fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, String> {
        self.inner
            .allocate_node_raw_log_path(&self.builder, &self.name, &self.build_key, label)
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

fn create_run_log_file(dir: &Path, base: &str) -> Result<(PathBuf, File), String> {
    for attempt in 1..1000 {
        let suffix = if attempt == 1 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let path = dir.join(format!("{base}{suffix}.jsonl"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create run log '{}': {error}",
                    path.display()
                ));
            }
        }
    }

    Err("exhausted run log filename attempts".to_string())
}

impl fmt::Display for RunOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RunOptions {{ emit_progress: {} }}", self.emit_progress)
    }
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
        let logger = Arc::new(BuildRunLogger::new(temp.path(), RunOptions::default()).unwrap());
        let build_key = "1111111111111111111111111111111111111111111111111111111111111111";
        let node_logger = logger.bind_node("Sandbox", "bash", build_key);

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
    }

    #[test]
    fn bound_logger_allocates_raw_logs_under_sanitized_build_name() {
        let temp = tempdir().unwrap();
        let logger = Arc::new(BuildRunLogger::new(temp.path(), RunOptions::default()).unwrap());
        let build_key = "2222222222222222222222222222222222222222222222222222222222222222";
        let node_logger = logger.bind_node("Sandbox", "bash debug/test", build_key);

        let path = node_logger.allocate_raw_log_path("podman/run").unwrap();
        let expected_dir = temp
            .path()
            .join("builder-state")
            .join("sandbox")
            .join("logs")
            .join("bash_debug_test");
        assert!(path.starts_with(&expected_dir));
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
