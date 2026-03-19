use mbuild_core::{BuildKey, BuildLogEvent, BuildLogger, ObjectHash, fsutil};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use time::OffsetDateTime;
use time::UtcOffset;
use time::macros::format_description;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOptions {
    pub emit_progress: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            emit_progress: false,
        }
    }
}

#[derive(Debug)]
pub struct BuildRunLogger {
    store_root: PathBuf,
    event_log_path: PathBuf,
    emit_progress: bool,
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

        let timestamp = human_timestamp();
        let pid = std::process::id();
        let (event_log_path, file) = create_run_log_file(&runs_dir, &format!("{timestamp}-{pid}"))
            .map_err(|error| {
                format!(
                    "failed to create event log under '{}': {error}",
                    runs_dir.display()
                )
            })?;

        Ok(Self {
            store_root: store_root.to_path_buf(),
            event_log_path,
            emit_progress: options.emit_progress,
            writer: Mutex::new(BufWriter::new(file)),
            raw_log_counters: Mutex::new(BTreeMap::new()),
        })
    }

    fn write_event(&self, event: &BuildLogEvent) -> Result<(), String> {
        let mut writer = self.writer.lock().map_err(|error| error.to_string())?;
        let line = serde_json::to_string(&EventLogRecord::from_event(event))
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
}

impl BuildLogger for BuildRunLogger {
    fn log_event(&self, event: BuildLogEvent) {
        if self.emit_progress {
            eprintln!("{}", format_progress_line(&event));
        }

        if let Err(error) = self.write_event(&event) {
            eprintln!("warning: {error}");
        }
    }

    fn allocate_raw_log_path(
        &self,
        builder: &str,
        name: &str,
        build_key: BuildKey,
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

        let timestamp = human_timestamp();
        let short_build_key = short_build_key(build_key);
        let base = format!(
            "{}-{}-{}",
            timestamp,
            short_build_key,
            sanitize_component(label)
        );
        unique_path(&logs_dir, &base, "log", &self.raw_log_counters)
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
    fn from_event(event: &BuildLogEvent) -> Self {
        let mut details = event.details.clone();
        details.insert(
            "full_build_key".to_string(),
            Value::String(event.build_key.to_string()),
        );
        if let Some(object_hash) = event.object_hash {
            details.insert(
                "full_object_hash".to_string(),
                Value::String(object_hash.to_string()),
            );
        }

        Self {
            ts: human_timestamp(),
            level: event.level.to_string(),
            phase: event.phase.clone(),
            builder: event.builder.clone(),
            name: event.name.clone(),
            build_key: short_build_key(event.build_key),
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

fn format_progress_line(event: &BuildLogEvent) -> String {
    let mut line = format!(
        "[{}] {} {} {}",
        event.phase,
        event.builder,
        event.name,
        short_build_key(event.build_key)
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

fn short_build_key(build_key: BuildKey) -> String {
    build_key.to_hex().chars().take(12).collect()
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

fn human_timestamp() -> String {
    let now = OffsetDateTime::from_unix_timestamp_nanos(
        (fsutil::current_epoch_nanos().unwrap_or(0) as i128)
            .try_into()
            .unwrap_or_default(),
    )
    .unwrap_or_else(|_| OffsetDateTime::now_utc());
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
