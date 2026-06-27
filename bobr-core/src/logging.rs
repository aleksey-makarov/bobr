use crate::ObjectHash;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use time::macros::format_description;

/// Schema tag stamped on every on-disk event record.
pub const BUILD_EVENT_SCHEMA: &str = "bobr-build-event-v1";

/// Severity of an event. Ordered `Info < Warn < Error`; the stderr progress
/// sink compares each event's level against its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuildLogLevel {
    Info,
    Warn,
    Error,
}

impl fmt::Display for BuildLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BuildLogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl Serialize for BuildLogLevel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Closed lifecycle status shared by every subject and run-level event.
///
/// Subject lifecycle is `start → (cache-hit | cache-miss → running → done) |
/// failed | cancelled`, plus `cleanup`. Builder-specific operations ride inside
/// `running` and are named by the free-form [`BuildLogEvent::op`] field, not by
/// this enum. `run-started`/`run-finished` describe the whole run, not a single
/// subject; `cache-hit` is a per-subject outcome surfaced on the run-level
/// channel (no workspace exists for a hit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStatus {
    Start,
    CacheHit,
    CacheMiss,
    Running,
    Done,
    Failed,
    Cancelled,
    Cleanup,
    RunStarted,
    RunFinished,
}

impl fmt::Display for BuildStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BuildStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::CacheHit => "cache-hit",
            Self::CacheMiss => "cache-miss",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Cleanup => "cleanup",
            Self::RunStarted => "run-started",
            Self::RunFinished => "run-finished",
        }
    }
}

/// Event payload produced by a source (builder, scheduler, runtime function).
///
/// The source supplies only the payload: `level`, `status`, `op`, `message`,
/// `object_hash`, `raw_log_path`, and `details`. Subject identity and the
/// envelope (`seq`/`subject_seq`/`ts`) are added by the logger when the
/// event is emitted, so a source can neither forge nor omit them.
#[derive(Debug, Clone)]
pub struct BuildLogEvent {
    pub level: BuildLogLevel,
    pub status: BuildStatus,
    pub op: Option<String>,
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

/// A consumer of fully assembled event records.
///
/// Every event is built once at the fan-out point and handed to each sink. The
/// file sink persists records to `events.jsonl`; the progress sink renders them
/// to stderr. New sinks (metrics, live progress) plug in without touching the
/// event producers.
pub trait EventSink: fmt::Debug + Send + Sync {
    /// Consumes one fully assembled, envelope-stamped record.
    fn write_event(&self, record: &EventLogRecord);

    /// Notifies the sink that a subject has been bound, before any of its
    /// events arrive. Sinks that keep per-subject state (file writers) set it
    /// up here; the default is a no-op.
    fn register_subject(&self, _subject: &BuildLogSubject) -> Result<(), String> {
        Ok(())
    }

    /// Flushes any buffered output to the OS. Default no-op for sinks that do
    /// not buffer.
    fn flush(&self) {}

    /// Number of write/flush/sync failures this sink has swallowed. Logging is
    /// best-effort and never fails the build, so failures are counted here
    /// instead of propagated. Default 0.
    fn error_count(&self) -> u64 {
        0
    }
}

/// The event bus: stamps the envelope once and fans every event out to sinks.
///
/// `BuildRunLogger` no longer writes files or stderr itself; it owns the sink
/// list and the run-global monotonic `seq` counter. `FileSink`/`ProgressSink`
/// do the actual work.
#[derive(Debug)]
pub struct BuildRunLogger {
    run_log_dir: PathBuf,
    run_id: String,
    seq: AtomicU64,
    sinks: Vec<Arc<dyn EventSink>>,
}

impl BuildRunLogger {
    pub fn new(run_log_dir: &Path, run_id: &str, quiet: bool) -> Result<Self, String> {
        fs::create_dir_all(run_log_dir).map_err(|error| {
            format!(
                "failed to create run log directory '{}': {error}",
                run_log_dir.display()
            )
        })?;

        let file_sink = Arc::new(FileSink::new(run_log_dir)?);
        let progress_sink = Arc::new(ProgressSink::new(run_log_dir.to_path_buf(), quiet));

        Ok(Self {
            run_log_dir: run_log_dir.to_path_buf(),
            run_id: run_id.to_string(),
            seq: AtomicU64::new(0),
            sinks: vec![file_sink, progress_sink],
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn run_log_dir(&self) -> &Path {
        &self.run_log_dir
    }

    /// Flushes every sink's buffered output to the OS. Routine events are
    /// buffered (see `FileSink`); this forces them out without waiting for a
    /// `Warn`/`Error`, the run-finished event, or sink drop.
    pub fn flush(&self) {
        for sink in &self.sinks {
            sink.flush();
        }
    }

    /// Total number of best-effort logging failures swallowed across sinks.
    /// Surfaced in the run-finished summary; logging never fails the build.
    pub fn logging_errors(&self) -> u64 {
        self.sinks.iter().map(|sink| sink.error_count()).sum()
    }

    pub fn bind_subject(
        self: &Arc<Self>,
        subject: BuildLogSubject,
    ) -> Result<Arc<dyn BuildLogger>, String> {
        for sink in &self.sinks {
            sink.register_subject(&subject)?;
        }
        Ok(Arc::new(BoundBuildLogger {
            inner: self.clone(),
            subject,
            subject_seq: AtomicU64::new(0),
            raw_log_counters: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Logs a run-level event that belongs to no single subject (build start,
    /// build finish, scheduler errors). The record has no `subject` block.
    pub fn log_run_event(&self, event: BuildLogEvent) {
        self.emit(None, None, &event);
    }

    /// Logs an event carrying a subject's identity but bound to no per-subject
    /// log. Used for cache hits, which have identity but no workspace: the
    /// record lands in the run-level log only (no subject writer is registered).
    pub fn log_subject_event(&self, identity: &SubjectIdentity, event: BuildLogEvent) {
        self.emit(Some(identity), None, &event);
    }

    /// Stamps the envelope once and fans the assembled record out to all sinks.
    fn emit(
        &self,
        subject: Option<&SubjectIdentity>,
        subject_seq: Option<u64>,
        event: &BuildLogEvent,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let record = EventLogRecord::assemble(seq, subject_seq, subject, event, &self.run_log_dir);
        for sink in &self.sinks {
            sink.write_event(&record);
        }
    }
}

/// Identity of one concrete builder or source subject, independent of its log
/// directories. Carried in every subject event, and also used for cache-hit
/// events, which have an identity but no workspace.
#[derive(Debug, Clone)]
pub struct SubjectIdentity {
    tag: String,
    name: String,
    build_key: String,
}

impl SubjectIdentity {
    pub fn new(
        tag: impl Into<String>,
        name: impl Into<String>,
        build_key: impl Into<String>,
    ) -> Self {
        Self {
            tag: tag.into(),
            name: name.into(),
            build_key: build_key.into(),
        }
    }
}

/// Subject identity plus the log directories allocated for a concrete run.
#[derive(Debug, Clone)]
pub struct BuildLogSubject {
    identity: SubjectIdentity,
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
            identity: SubjectIdentity::new(tag, name, build_key),
            log_dir,
            raw_log_dir,
        }
    }

    /// Returns the subject's identity (tag, name, build key).
    pub fn identity(&self) -> &SubjectIdentity {
        &self.identity
    }
}

#[derive(Debug)]
struct BoundBuildLogger {
    inner: Arc<BuildRunLogger>,
    subject: BuildLogSubject,
    subject_seq: AtomicU64,
    raw_log_counters: Mutex<BTreeMap<String, usize>>,
}

impl BuildLogger for BoundBuildLogger {
    fn log_event(&self, event: BuildLogEvent) {
        let subject_seq = self.subject_seq.fetch_add(1, Ordering::Relaxed);
        self.inner
            .emit(Some(self.subject.identity()), Some(subject_seq), &event);
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

/// File sink: persists records to the run-level and per-subject `events.jsonl`.
///
/// Owns the run-level writer plus a writer per bound subject (keyed by the full
/// build key). A subject event is written to both the run log and that
/// subject's log; the serialized line is identical in both, so tooling can
/// match records byte-for-byte.
#[derive(Debug)]
struct FileSink {
    run_event_log_path: PathBuf,
    run_writer: Mutex<BufWriter<File>>,
    subject_writers: Mutex<HashMap<String, SubjectWriter>>,
    errors: AtomicU64,
}

#[derive(Debug)]
struct SubjectWriter {
    event_log_path: PathBuf,
    writer: BufWriter<File>,
}

impl FileSink {
    fn new(run_log_dir: &Path) -> Result<Self, String> {
        let run_event_log_path = run_log_dir.join("events.jsonl");
        let file = create_event_log_file(&run_event_log_path).map_err(|error| {
            format!(
                "failed to create run event log '{}': {error}",
                run_event_log_path.display()
            )
        })?;
        Ok(Self {
            run_event_log_path,
            run_writer: Mutex::new(BufWriter::new(file)),
            subject_writers: Mutex::new(HashMap::new()),
            errors: AtomicU64::new(0),
        })
    }

    /// Appends one line to the buffer **without** flushing. Routine events stay
    /// buffered; flushing is decided per record in `write_event`.
    fn append(writer: &mut BufWriter<File>, line: &str, path: &Path) -> Result<(), String> {
        writer
            .write_all(line.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .map_err(|error| format!("failed to append event log '{}': {error}", path.display()))
    }

    /// Flushes the buffer to the OS; when `sync`, also fsyncs the file to disk.
    fn flush_writer(writer: &mut BufWriter<File>, sync: bool, path: &Path) -> Result<(), String> {
        writer
            .flush()
            .and_then(|_| {
                if sync {
                    writer.get_ref().sync_data()
                } else {
                    Ok(())
                }
            })
            .map_err(|error| format!("failed to flush event log '{}': {error}", path.display()))
    }

    /// Records a best-effort logging failure: counts it and warns, never fails
    /// the build.
    fn note_error(&self, message: String) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        eprintln!("warning: {message}");
    }
}

impl EventSink for FileSink {
    fn write_event(&self, record: &EventLogRecord) {
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(error) => {
                self.note_error(format!("failed to serialize build event: {error}"));
                return;
            }
        };

        // Routine `Info` events stay buffered. Flush `Warn`/`Error` (and the
        // terminal run event) so anything diagnostically relevant survives a
        // process crash; fsync only the run log on `run-finished`.
        let run_finished = record.status.as_str() == BuildStatus::RunFinished.as_str();
        let flush_now = record.level >= BuildLogLevel::Warn || run_finished;

        match self.run_writer.lock() {
            Ok(mut writer) => {
                if let Err(error) = Self::append(&mut writer, &line, &self.run_event_log_path) {
                    self.note_error(error);
                } else if flush_now
                    && let Err(error) =
                        Self::flush_writer(&mut writer, run_finished, &self.run_event_log_path)
                {
                    self.note_error(error);
                }
            }
            Err(error) => self.note_error(error.to_string()),
        }

        let Some(subject) = &record.subject else {
            return;
        };
        match self.subject_writers.lock() {
            Ok(mut writers) => {
                if let Some(subject_writer) = writers.get_mut(&subject.build_key) {
                    if let Err(error) = Self::append(
                        &mut subject_writer.writer,
                        &line,
                        &subject_writer.event_log_path,
                    ) {
                        self.note_error(error);
                    } else if flush_now
                        && let Err(error) = Self::flush_writer(
                            &mut subject_writer.writer,
                            false,
                            &subject_writer.event_log_path,
                        )
                    {
                        self.note_error(error);
                    }
                }
            }
            Err(error) => self.note_error(error.to_string()),
        }
    }

    fn flush(&self) {
        if let Ok(mut writer) = self.run_writer.lock()
            && let Err(error) = Self::flush_writer(&mut writer, false, &self.run_event_log_path)
        {
            self.note_error(error);
        }
        if let Ok(mut writers) = self.subject_writers.lock() {
            for subject_writer in writers.values_mut() {
                if let Err(error) = Self::flush_writer(
                    &mut subject_writer.writer,
                    false,
                    &subject_writer.event_log_path,
                ) {
                    self.note_error(error);
                }
            }
        }
    }

    fn error_count(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    fn register_subject(&self, subject: &BuildLogSubject) -> Result<(), String> {
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
        let file = create_event_log_file(&event_log_path).map_err(|error| {
            format!(
                "failed to create subject event log '{}': {error}",
                event_log_path.display()
            )
        })?;
        let mut writers = self
            .subject_writers
            .lock()
            .map_err(|error| error.to_string())?;
        writers.insert(
            subject.identity.build_key.clone(),
            SubjectWriter {
                event_log_path,
                writer: BufWriter::new(file),
            },
        );
        Ok(())
    }
}

/// Progress sink: renders events to stderr as the build's live UI.
///
/// The only writer of stderr progress. Events below `min_level` are dropped
/// from stderr: `quiet` raises the threshold to `Warn`, so only warnings and
/// errors show and progress is silenced; otherwise the threshold is `Info` and
/// everything shows. `Warn`/`Error` are never suppressed by construction, and
/// file logs are unaffected (they record every level). Raw-log paths are stored
/// run-relative in the record, so this sink rejoins `run_log_dir` to show an
/// absolute path on screen.
#[derive(Debug)]
struct ProgressSink {
    run_log_dir: PathBuf,
    min_level: BuildLogLevel,
}

impl ProgressSink {
    fn new(run_log_dir: PathBuf, quiet: bool) -> Self {
        Self {
            run_log_dir,
            min_level: stderr_min_level(quiet),
        }
    }
}

/// Lowest level shown on stderr. `quiet` raises the bar to `Warn` (progress
/// silenced, warnings/errors still shown); otherwise everything from `Info` up.
fn stderr_min_level(quiet: bool) -> BuildLogLevel {
    if quiet {
        BuildLogLevel::Warn
    } else {
        BuildLogLevel::Info
    }
}

impl EventSink for ProgressSink {
    fn write_event(&self, record: &EventLogRecord) {
        if record.level >= self.min_level {
            eprintln!("{}", format_progress_line(record, &self.run_log_dir));
        }
    }
}

#[derive(Debug, Serialize)]
pub struct EventLogRecord {
    schema: &'static str,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_seq: Option<u64>,
    ts: String,
    level: BuildLogLevel,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    op: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<SubjectRecord>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_log: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    details: Map<String, Value>,
}

#[derive(Debug, Serialize)]
struct SubjectRecord {
    tag: String,
    name: String,
    // Full, canonical values. The 12-char short forms are derivable by
    // truncation and are computed only for the progress line, not stored.
    build_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    object_hash: Option<String>,
}

impl EventLogRecord {
    fn assemble(
        seq: u64,
        subject_seq: Option<u64>,
        subject: Option<&SubjectIdentity>,
        event: &BuildLogEvent,
        run_log_dir: &Path,
    ) -> Self {
        let subject = subject.map(|subject| SubjectRecord {
            tag: subject.tag.clone(),
            name: subject.name.clone(),
            build_key: subject.build_key.clone(),
            object_hash: event.object_hash.map(|hash| hash.to_string()),
        });

        let raw_log = event
            .raw_log_path
            .as_ref()
            .map(|path| relativize_raw_log(path, run_log_dir));

        Self {
            schema: BUILD_EVENT_SCHEMA,
            seq,
            subject_seq,
            ts: current_timestamp_rfc3339(),
            level: event.level,
            status: event.status.as_str().to_string(),
            op: event.op.clone(),
            subject,
            message: event.message.clone(),
            raw_log,
            details: event.details.clone(),
        }
    }
}

fn format_progress_line(record: &EventLogRecord, run_log_dir: &Path) -> String {
    let label = record.op.as_deref().unwrap_or(record.status.as_str());
    let mut line = format!("[{label}]");

    if let Some(subject) = &record.subject {
        line.push(' ');
        line.push_str(&subject.tag);
        line.push(' ');
        line.push_str(&subject.name);
        line.push(' ');
        line.push_str(&short_id(&subject.build_key));
        if let Some(object_hash) = &subject.object_hash {
            line.push(' ');
            line.push_str(&short_id(object_hash));
        }
    }

    if !record.message.is_empty() {
        line.push_str(": ");
        line.push_str(&record.message);
    }

    if let Some(raw_log) = &record.raw_log {
        line.push_str(" (log: ");
        line.push_str(&run_log_dir.join(raw_log).display().to_string());
        line.push(')');
    }

    line
}

fn relativize_raw_log(path: &Path, run_log_dir: &Path) -> String {
    path.strip_prefix(run_log_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// 12-char prefix of a build key or object hash, for the progress line only.
fn short_id(value: &str) -> String {
    value.chars().take(12).collect()
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

fn current_timestamp_rfc3339() -> String {
    let now = OffsetDateTime::now_utc();
    let format =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    now.format(&format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_string())
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

    fn run_event_log(run_log_dir: &Path) -> String {
        fs::read_to_string(run_log_dir.join("events.jsonl")).unwrap()
    }

    #[test]
    fn quiet_stderr_threshold_drops_progress_but_keeps_warnings() {
        // quiet: only Warn/Error reach stderr; Info (progress) is dropped.
        let quiet = stderr_min_level(true);
        assert!(BuildLogLevel::Info < quiet);
        assert!(BuildLogLevel::Warn >= quiet);
        assert!(BuildLogLevel::Error >= quiet);

        // normal: everything from Info up reaches stderr.
        let normal = stderr_min_level(false);
        assert!(BuildLogLevel::Info >= normal);
        assert!(BuildLogLevel::Warn >= normal);
        assert!(BuildLogLevel::Error >= normal);
    }

    #[test]
    fn bound_logger_writes_subject_identity_and_envelope() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
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
            status: BuildStatus::Start,
            op: None,
            message: "starting builder node".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });

        logger.flush();
        let contents = run_event_log(&run_log_dir);
        let line = contents.lines().last().unwrap();
        let event: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            event["schema"],
            Value::String(BUILD_EVENT_SCHEMA.to_string())
        );
        // run_id is the run directory name, not duplicated into every line.
        assert!(event.get("run_id").is_none());
        assert_eq!(event["seq"], Value::from(0));
        assert_eq!(event["subject_seq"], Value::from(0));
        assert_eq!(event["status"], Value::String("start".to_string()));
        assert!(event.get("op").is_none());
        assert_eq!(
            event["subject"]["tag"],
            Value::String("Sandbox".to_string())
        );
        assert_eq!(event["subject"]["name"], Value::String("bash".to_string()));
        // build_key holds the full, canonical value (no separate short field).
        assert_eq!(
            event["subject"]["build_key"],
            Value::String(build_key.to_string())
        );

        let subject_contents = fs::read_to_string(subject_dir.join("events.jsonl")).unwrap();
        let subject_line = subject_contents.lines().last().unwrap();
        // The run-level and subject-level lines are byte-identical.
        assert_eq!(subject_line, line);
    }

    #[test]
    fn op_events_carry_running_status_and_op_field() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );
        let build_key = "4444444444444444444444444444444444444444444444444444444444444444";
        let subject_dir = run_log_dir.join("00000000-Erofs-image");
        let subject = BuildLogSubject::new(
            "Erofs",
            "image",
            build_key,
            subject_dir.clone(),
            subject_dir.join("raw"),
        );
        let node_logger = logger.bind_subject(subject).unwrap();

        node_logger.log_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::Running,
            op: Some("mkfs".to_string()),
            message: "creating EROFS image".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });

        logger.flush();
        let contents = run_event_log(&run_log_dir);
        let event: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert_eq!(event["status"], Value::String("running".to_string()));
        assert_eq!(event["op"], Value::String("mkfs".to_string()));
    }

    #[test]
    fn subject_seq_is_monotonic_per_subject() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );
        let build_key = "5555555555555555555555555555555555555555555555555555555555555555";
        let subject_dir = run_log_dir.join("00000000-Tree-pkg");
        let subject = BuildLogSubject::new(
            "Tree",
            "pkg",
            build_key,
            subject_dir.clone(),
            subject_dir.join("raw"),
        );
        let node_logger = logger.bind_subject(subject).unwrap();

        for index in 0..3 {
            node_logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::Running,
                op: Some("stage".to_string()),
                message: format!("step {index}"),
                object_hash: None,
                raw_log_path: None,
                details: Map::new(),
            });
        }

        logger.flush();
        let contents = run_event_log(&run_log_dir);
        let seqs: Vec<u64> = contents
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["subject_seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[test]
    fn run_level_event_has_no_subject() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );

        logger.log_run_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::RunStarted,
            op: None,
            message: "build started".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });

        logger.flush();
        let contents = run_event_log(&run_log_dir);
        let event: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert_eq!(event["status"], Value::String("run-started".to_string()));
        assert!(event.get("subject").is_none());
        assert!(event.get("subject_seq").is_none());
    }

    #[test]
    fn info_events_are_buffered_until_flush() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );

        logger.log_run_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::RunStarted,
            op: None,
            message: "build started".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });

        // Routine Info stays buffered: nothing on disk until an explicit flush.
        assert!(run_event_log(&run_log_dir).is_empty());
        logger.flush();
        assert!(!run_event_log(&run_log_dir).is_empty());
    }

    #[test]
    fn warn_events_are_flushed_without_explicit_flush() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );

        // status is orthogonal here; the point is that Warn triggers a flush.
        logger.log_run_event(BuildLogEvent {
            level: BuildLogLevel::Warn,
            status: BuildStatus::RunStarted,
            op: None,
            message: "heads up".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });

        let event: Value =
            serde_json::from_str(run_event_log(&run_log_dir).lines().last().unwrap()).unwrap();
        assert_eq!(event["level"], Value::String("warn".to_string()));
    }

    #[test]
    fn run_finished_flushes_run_log_without_explicit_flush() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );

        logger.log_run_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::RunFinished,
            op: None,
            message: "build finished".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });

        // The terminal run event is flushed (and fsynced) without an explicit flush.
        let event: Value =
            serde_json::from_str(run_event_log(&run_log_dir).lines().last().unwrap()).unwrap();
        assert_eq!(event["status"], Value::String("run-finished".to_string()));
    }

    #[test]
    fn logging_errors_starts_at_zero() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger =
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap();
        assert_eq!(logger.logging_errors(), 0);
    }

    #[test]
    fn cache_hit_event_carries_identity_but_writes_no_subject_file() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );
        let build_key = "7777777777777777777777777777777777777777777777777777777777777777";
        let object_hash: ObjectHash =
            "8888888888888888888888888888888888888888888888888888888888888888"
                .parse()
                .unwrap();
        let identity = SubjectIdentity::new("Tree", "cached", build_key);

        // No bind_subject: a cache hit has no workspace, so no subject writer.
        logger.log_subject_event(
            &identity,
            BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::CacheHit,
                op: None,
                message: "served from cache".to_string(),
                object_hash: Some(object_hash),
                raw_log_path: None,
                details: Map::new(),
            },
        );

        logger.flush();
        let contents = run_event_log(&run_log_dir);
        let event: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert_eq!(event["status"], Value::String("cache-hit".to_string()));
        assert_eq!(event["subject"]["tag"], Value::String("Tree".to_string()));
        assert_eq!(
            event["subject"]["build_key"],
            Value::String(build_key.to_string())
        );
        assert_eq!(
            event["subject"]["object_hash"],
            Value::String(object_hash.to_string())
        );
        // The hit lands only in the run-level log; there is no subject directory.
        assert!(!run_log_dir.join("00000000-Tree-cached").exists());
    }

    #[test]
    fn raw_log_path_is_run_relative() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );
        let build_key = "6666666666666666666666666666666666666666666666666666666666666666";
        let subject_dir = run_log_dir.join("00000000-Sandbox-bash");
        let raw_dir = subject_dir.join("raw");
        let subject = BuildLogSubject::new(
            "Sandbox",
            "bash",
            build_key,
            subject_dir.clone(),
            raw_dir.clone(),
        );
        let node_logger = logger.bind_subject(subject).unwrap();

        node_logger.log_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::Running,
            op: Some("sandbox-result".to_string()),
            message: "sandbox wrote manifest".to_string(),
            object_hash: None,
            raw_log_path: Some(raw_dir.join("sandbox-result.log")),
            details: Map::new(),
        });

        logger.flush();
        let contents = run_event_log(&run_log_dir);
        let event: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert_eq!(
            event["raw_log"],
            Value::String("00000000-Sandbox-bash/raw/sandbox-result.log".to_string())
        );
    }

    #[test]
    fn bound_logger_allocates_raw_logs_under_subject_raw_dir() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
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
