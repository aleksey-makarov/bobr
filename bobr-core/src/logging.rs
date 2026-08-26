use crate::ObjectHash;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::macros::format_description;

/// Schema tag stamped on every on-disk event record.
pub const BUILD_EVENT_SCHEMA: &str = "bobr-build-event-v1";

/// Minimum total height accepted for a fixed live progress block.
pub const MIN_FIXED_PROGRESS_LINES: usize = 4;

/// Presentation policy carried by a unified realization request.
///
/// This controls only terminal rendering. It never participates in build or
/// reuse identity, and non-terminal output remains plain for every variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgressPolicy {
    /// Derive a line budget from the current terminal height.
    #[default]
    Auto,
    /// Keep fetch/build statistics and the run summary, but no activity rows.
    Summary,
    /// Cap the complete live block at `max_lines` terminal rows.
    Fixed {
        /// Maximum rows occupied by both statistics, activity rows, and the
        /// run summary together.
        max_lines: usize,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProgressModeWire {
    Auto,
    Summary,
    Fixed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgressPolicyWire {
    mode: ProgressModeWire,
    max_lines: Option<usize>,
}

impl<'de> Deserialize<'de> for ProgressPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ProgressPolicyWire::deserialize(deserializer)?;
        match (wire.mode, wire.max_lines) {
            (ProgressModeWire::Auto, None) => Ok(Self::Auto),
            (ProgressModeWire::Summary, None) => Ok(Self::Summary),
            (ProgressModeWire::Fixed, Some(max_lines)) => Ok(Self::Fixed { max_lines }),
            (ProgressModeWire::Fixed, None) => Err(serde::de::Error::missing_field("max_lines")),
            (ProgressModeWire::Auto | ProgressModeWire::Summary, Some(_)) => Err(
                serde::de::Error::custom("progress max_lines is allowed only in fixed mode"),
            ),
        }
    }
}

impl ProgressPolicy {
    /// Rejects fixed budgets too small to hold a useful live block.
    pub fn validate(self) -> Result<(), String> {
        if let Self::Fixed { max_lines } = self
            && max_lines < MIN_FIXED_PROGRESS_LINES
        {
            return Err(format!(
                "progress fixed max_lines must be at least {MIN_FIXED_PROGRESS_LINES}"
            ));
        }
        Ok(())
    }
}

/// Severity of an event. Ordered `Progress < Info < Warn < Error`; the stderr
/// progress sink compares each event's level against its threshold.
///
/// `Progress` is a transient, screen-only level: it is rendered to stderr (in
/// non-quiet mode) but **never persisted** — `FileSink` drops it, so the
/// on-disk level vocabulary stays `info`/`warn`/`error`. Use it for
/// high-frequency progress ticks (e.g. download byte counts); use `Info` for
/// durable milestones worth keeping in the event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuildLogLevel {
    /// Transient progress tick: shown on screen, never persisted.
    Progress,
    /// Routine informational milestone.
    Info,
    /// Warning; surfaced even in quiet mode.
    Warn,
    /// Error; surfaced even in quiet mode.
    Error,
}

impl fmt::Display for BuildLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BuildLogLevel {
    /// The lowercase wire string for this level (`"progress"`, `"info"`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Progress => "progress",
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
    /// A subject began execution.
    Start,
    /// A subject was served from cache (run-level outcome; no workspace).
    CacheHit,
    /// A cache miss: the subject must be built.
    CacheMiss,
    /// The subject's builder is running.
    Running,
    /// The subject completed successfully.
    Done,
    /// The subject failed.
    Failed,
    /// The subject was cancelled before completing.
    Cancelled,
    /// Post-execution cleanup (e.g. removing the temp dir).
    Cleanup,
    /// The whole run started.
    RunStarted,
    /// The whole run finished.
    RunFinished,
}

impl fmt::Display for BuildStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BuildStatus {
    /// The kebab-case wire string for this status (`"cache-hit"`,
    /// `"run-started"`, …).
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
    /// Severity level.
    pub level: BuildLogLevel,
    /// Lifecycle status.
    pub status: BuildStatus,
    /// Optional named operation within `running` (free-form).
    pub op: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// Realized object hash, when the event reports one.
    pub object_hash: Option<ObjectHash>,
    /// Path to an associated raw log file, if any.
    pub raw_log_path: Option<PathBuf>,
    /// Extra structured fields.
    pub details: Map<String, Value>,
}

/// Sink that a subject (or the run) logs events to. Implementors stamp the
/// envelope (sequence numbers, timestamp) and fan events out to the configured
/// sinks.
pub trait BuildLogger: fmt::Debug + Send + Sync {
    /// Emits one event; the caller supplies only the payload, the logger adds
    /// subject identity and the envelope.
    fn log_event(&self, event: BuildLogEvent);

    /// Allocates a fresh path for a raw log file labelled `label`, erroring if
    /// this logger has no workspace to write into.
    fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, String>;
}

/// A [`BuildLogger`] that discards everything. Used where no per-subject log
/// exists (e.g. cache hits).
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

    /// Notifies the sink that a subject is done and no further events for it
    /// will arrive, so any per-subject state can be released. Called when the
    /// subject's logger is dropped, which covers success, failure and
    /// cancellation alike. The default is a no-op.
    fn release_subject(&self, _identity: &SubjectIdentity) {}

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

/// Counts download retries, by host and by what went wrong.
///
/// Retries are reported per subject, as they happen, which says nothing about
/// whether one host was behind all of them. Counting here -- where every event
/// already passes -- turns that into a fact the run can report, without a
/// counter threaded through the source layer or a global to hold it.
///
/// The reason is counted alongside because the two answer different questions:
/// the host says who to ask for less, the reason says whether asking less is
/// even the fix.
#[derive(Debug, Default)]
struct RetrySink {
    hosts: Mutex<BTreeMap<String, u64>>,
    reasons: Mutex<BTreeMap<String, u64>>,
}

/// Exact terminal outcomes observed by the run logger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RunOutcomeStats {
    /// Builders whose staged output was published.
    pub built: u64,
    /// Subjects accepted from exact/reuse/cache content.
    pub cache_hit: u64,
    /// Subjects that terminated with failure.
    pub failed: u64,
    /// Subjects cancelled before publication.
    pub cancelled: u64,
    /// Source objects downloaded over HTTP or OCI.
    pub downloaded: u64,
    /// Source objects materialized from a local Path origin.
    pub local: u64,
    /// Source objects acquired from secondary content.
    pub secondary: u64,
    /// Source objects already complete in the working store.
    pub already_present: u64,
}

#[derive(Debug, Default)]
struct OutcomeSink {
    built: AtomicU64,
    cache_hit: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    downloaded: AtomicU64,
    local: AtomicU64,
    secondary: AtomicU64,
    already_present: AtomicU64,
}

impl OutcomeSink {
    fn snapshot(&self) -> RunOutcomeStats {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        RunOutcomeStats {
            built: load(&self.built),
            cache_hit: load(&self.cache_hit),
            failed: load(&self.failed),
            cancelled: load(&self.cancelled),
            downloaded: load(&self.downloaded),
            local: load(&self.local),
            secondary: load(&self.secondary),
            already_present: load(&self.already_present),
        }
    }
}

impl EventSink for OutcomeSink {
    fn write_event(&self, record: &EventLogRecord) {
        let Some(subject) = &record.subject else {
            return;
        };
        match record.status.as_str() {
            "done" if subject.tag != "Source" && subject.tag != "SecondaryContent" => {
                self.built.fetch_add(1, Ordering::Relaxed);
            }
            "cache-hit" => {
                self.cache_hit.fetch_add(1, Ordering::Relaxed);
            }
            "failed" => {
                self.failed.fetch_add(1, Ordering::Relaxed);
            }
            "cancelled" => {
                self.cancelled.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let Some(outcome) = record.details.get("source_outcome").and_then(Value::as_str) else {
            return;
        };
        let counter = match outcome {
            "downloaded" => &self.downloaded,
            "local" => &self.local,
            "secondary" => &self.secondary,
            "already_present" => &self.already_present,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl RetrySink {
    fn counts(&self) -> BTreeMap<String, u64> {
        self.hosts
            .lock()
            .map(|hosts| hosts.clone())
            .unwrap_or_default()
    }

    fn reason_counts(&self) -> BTreeMap<String, u64> {
        self.reasons
            .lock()
            .map(|reasons| reasons.clone())
            .unwrap_or_default()
    }
}

impl EventSink for RetrySink {
    fn write_event(&self, record: &EventLogRecord) {
        // The host is what marks an event as a retry; the reason is counted
        // only for events that carry one, so an older producer that sends no
        // reason still gets its retries counted by host.
        let Some(Value::String(host)) = record.details.get("retry_host") else {
            return;
        };
        if let Ok(mut hosts) = self.hosts.lock() {
            *hosts.entry(host.clone()).or_insert(0) += 1;
        }
        if let Some(Value::String(reason)) = record.details.get("retry_reason")
            && let Ok(mut reasons) = self.reasons.lock()
        {
            *reasons.entry(reason.clone()).or_insert(0) += 1;
        }
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
    progress: Arc<ProgressSink>,
    retries: Arc<RetrySink>,
    outcomes: Arc<OutcomeSink>,
}

impl BuildRunLogger {
    /// Creates a run logger writing under `run_log_dir`, tagged with `run_id`.
    /// `quiet` raises the stderr threshold (warnings/errors only). Sets up the
    /// file and progress sinks.
    pub fn new(run_log_dir: &Path, run_id: &str, quiet: bool) -> Result<Self, String> {
        Self::new_with_progress(run_log_dir, run_id, quiet, ProgressPolicy::Auto)
    }

    /// Creates a run logger with an explicit terminal presentation policy.
    pub fn new_with_progress(
        run_log_dir: &Path,
        run_id: &str,
        quiet: bool,
        progress_policy: ProgressPolicy,
    ) -> Result<Self, String> {
        fs::create_dir_all(run_log_dir).map_err(|error| {
            format!(
                "failed to create run log directory '{}': {error}",
                run_log_dir.display()
            )
        })?;

        let file_sink = Arc::new(FileSink::new(run_log_dir)?);
        let progress_sink = Arc::new(ProgressSink::new(
            run_log_dir.to_path_buf(),
            quiet,
            progress_policy,
        ));
        let retries = Arc::new(RetrySink::default());
        let outcomes = Arc::new(OutcomeSink::default());

        Ok(Self {
            run_log_dir: run_log_dir.to_path_buf(),
            run_id: run_id.to_string(),
            seq: AtomicU64::new(0),
            sinks: vec![
                file_sink,
                Arc::clone(&progress_sink) as Arc<dyn EventSink>,
                Arc::clone(&retries) as Arc<dyn EventSink>,
                Arc::clone(&outcomes) as Arc<dyn EventSink>,
            ],
            progress: progress_sink,
            retries,
            outcomes,
        })
    }

    /// Recomputes the live viewport after a terminal resize.
    pub fn refresh_progress_layout(&self) {
        self.progress.refresh_layout();
    }

    /// The run's identifier.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The directory this run's logs are written under.
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

    /// How many download retries this run made, by host.
    ///
    /// Empty when nothing had to be retried, which is the ordinary case.
    pub fn download_retries(&self) -> BTreeMap<String, u64> {
        self.retries.counts()
    }

    /// The same retries, by what they were answering -- `dns`, `http 5xx`,
    /// `timeout` and so on.
    ///
    /// Empty when nothing had to be retried.
    pub fn download_retry_reasons(&self) -> BTreeMap<String, u64> {
        self.retries.reason_counts()
    }

    /// Returns terminal run outcomes accumulated from structured events.
    pub fn outcome_stats(&self) -> RunOutcomeStats {
        self.outcomes.snapshot()
    }

    /// Total number of best-effort logging failures swallowed across sinks.
    /// Surfaced in the run-finished summary; logging never fails the build.
    pub fn logging_errors(&self) -> u64 {
        self.sinks.iter().map(|sink| sink.error_count()).sum()
    }

    /// Binds a subject to this run: registers it with every sink and returns a
    /// per-subject [`BuildLogger`] that stamps the subject's identity on its
    /// events.
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

    /// Releases every sink's state for a finished subject. Called from
    /// `BoundBuildLogger::drop`: the bound logger is the only handle through
    /// which subject events can be emitted, so once it is gone no further event
    /// can reach that subject's state.
    fn release_subject(&self, identity: &SubjectIdentity) {
        for sink in &self.sinks {
            sink.release_subject(identity);
        }
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
        // Progress events are transient and never persisted, so they must not
        // consume the durable run sequence — otherwise the on-disk `seq` would
        // show gaps that look like lost events. Peek instead of advancing.
        let seq = if event.level == BuildLogLevel::Progress {
            self.seq.load(Ordering::Relaxed)
        } else {
            self.seq.fetch_add(1, Ordering::Relaxed)
        };
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
    /// Creates a subject identity from its `tag`, `name`, and `build_key`.
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
        // Transient progress does not consume the durable per-subject sequence
        // (see `BuildRunLogger::emit`): peek instead of advancing.
        let subject_seq = if event.level == BuildLogLevel::Progress {
            self.subject_seq.load(Ordering::Relaxed)
        } else {
            self.subject_seq.fetch_add(1, Ordering::Relaxed)
        };
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

/// Releasing on drop rather than on a "subject finished" event is deliberate:
/// the executor drops this logger on every path out of a subject -- success,
/// builder failure, cancellation, panic -- so no path can leak the subject's
/// open log file. A run builds far more subjects than it runs concurrently, so
/// holding a file per *finished* subject would exhaust the process's descriptor
/// limit part-way through a large build.
impl Drop for BoundBuildLogger {
    fn drop(&mut self) {
        self.inner.release_subject(self.subject.identity());
    }
}

/// File sink: persists records to the run-level and per-subject `events.jsonl`.
///
/// Owns the run-level writer plus a writer per *live* subject (keyed by the
/// full build key). A subject event is written to both the run log and that
/// subject's log; the serialized line is identical in both, so tooling can
/// match records byte-for-byte.
///
/// A subject's writer is created by `register_subject` and closed again by
/// `release_subject`, so the number of open files tracks how many subjects are
/// running at once, not how many the run has built.
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
    /// Lines waiting to reach the file. The file itself is never held open:
    /// a build binds at most `jobs` subjects at a time, but the fetcher binds
    /// every source of its request at once, and one descriptor per bound
    /// subject was enough to exhaust the process limit mid-run.
    buffered: Vec<u8>,
}

impl SubjectWriter {
    fn append(&mut self, line: &str) {
        self.buffered.extend_from_slice(line.as_bytes());
        self.buffered.push(b'\n');
    }

    /// Opens the file, appends everything buffered, and closes it again.
    fn flush(&mut self, sync: bool) -> Result<(), String> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        let describe = |error: std::io::Error| {
            format!(
                "failed to append event log '{}': {error}",
                self.event_log_path.display()
            )
        };
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&self.event_log_path)
            .map_err(describe)?;
        file.write_all(&self.buffered).map_err(describe)?;
        if sync {
            file.sync_data().map_err(describe)?;
        }
        self.buffered.clear();
        Ok(())
    }
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
        // Progress is transient and screen-only: never persisted. Dropping it
        // before serialization keeps the on-disk level vocabulary to
        // info/warn/error and avoids the write cost entirely.
        if record.level == BuildLogLevel::Progress {
            return;
        }
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
                    subject_writer.append(&line);
                    if flush_now && let Err(error) = subject_writer.flush(false) {
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
                if let Err(error) = subject_writer.flush(false) {
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
        // Created (and truncated) here to claim the path, then closed at once:
        // appends reopen it. See SubjectWriter for why no handle is kept.
        create_event_log_file(&event_log_path).map_err(|error| {
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
                buffered: Vec::new(),
            },
        );
        Ok(())
    }

    /// Closes the subject's `events.jsonl`, flushing what is still buffered.
    ///
    /// Flushing explicitly rather than leaving it to `BufWriter::drop` is what
    /// makes a lost tail visible: drop swallows the error, this counts it.
    fn release_subject(&self, identity: &SubjectIdentity) {
        let removed = match self.subject_writers.lock() {
            Ok(mut writers) => writers.remove(&identity.build_key),
            Err(error) => {
                self.note_error(error.to_string());
                return;
            }
        };
        let Some(mut subject_writer) = removed else {
            return;
        };
        if let Err(error) = subject_writer.flush(false) {
            self.note_error(error);
        }
    }
}

/// Progress sink: the build's live UI on stderr (the only stderr writer).
///
/// In an interactive terminal (and not `quiet`) it renders a live block via
/// indicatif — one line per active subject, updated in place, with a summary
/// line at the bottom; `Warn`/`Error` print above the block. Otherwise (non-TTY
/// or `quiet`) it falls back to plain per-line output. Transient `Progress`
/// ticks are shown only in the live block — in plain mode they would be scroll
/// noise, so the plain threshold starts at `Info`. File logs are unaffected
/// (and never carry `Progress`).
enum ProgressSink {
    /// Live indicatif block (interactive terminal, non-quiet). Boxed: the live
    /// state is far larger than the plain variant.
    Live(Box<Mutex<LiveProgress>>),
    /// Plain per-line stderr at or above `min_level`.
    Plain {
        run_log_dir: PathBuf,
        min_level: BuildLogLevel,
        /// Present once a run asks for the aggregate shape. Off a terminal
        /// there is no block to redraw, so the same figures go out as a
        /// heartbeat -- a fetch run of eight hundred sources is two thousand
        /// lines of "fetching"/"fetched" otherwise, and a CI log or the
        /// rebuild-world transcript is unreadable for it.
        aggregate: Mutex<Option<PlainAggregate>>,
    },
}

/// Aggregate state for the plain path, with the moment it last spoke.
struct PlainAggregate {
    progress: FetchProgress,
    last_printed: Instant,
}

/// How often the plain path prints the aggregate figures.
const PLAIN_AGGREGATE_HEARTBEAT: Duration = Duration::from_secs(15);

impl fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Live(_) => f.write_str("ProgressSink::Live"),
            Self::Plain { min_level, .. } => {
                write!(f, "ProgressSink::Plain {{ min_level: {min_level} }}")
            }
        }
    }
}

impl ProgressSink {
    fn new(run_log_dir: PathBuf, quiet: bool, policy: ProgressPolicy) -> Self {
        if !quiet && std::io::stderr().is_terminal() {
            Self::Live(Box::new(Mutex::new(LiveProgress::new(
                run_log_dir,
                MultiProgress::new(),
                policy,
            ))))
        } else {
            Self::Plain {
                run_log_dir,
                min_level: stderr_min_level(quiet),
                aggregate: Mutex::new(None),
            }
        }
    }

    /// Live sink with a hidden draw target, for tests: exercises the adapter
    /// without touching a terminal.
    #[cfg(test)]
    fn live_hidden(run_log_dir: PathBuf) -> Self {
        Self::live_hidden_with_policy(run_log_dir, ProgressPolicy::Auto)
    }

    #[cfg(test)]
    fn live_hidden_with_policy(run_log_dir: PathBuf, policy: ProgressPolicy) -> Self {
        let multi = MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden());
        Self::Live(Box::new(Mutex::new(LiveProgress::new(
            run_log_dir,
            multi,
            policy,
        ))))
    }

    fn refresh_layout(&self) {
        if let Self::Live(state) = self
            && let Ok(mut live) = state.lock()
        {
            live.refresh_layout();
        }
    }
}

impl Drop for ProgressSink {
    fn drop(&mut self) {
        // Clear any remaining live block so a panic/early exit doesn't leave a
        // half-drawn block on the terminal.
        if let Self::Live(state) = self
            && let Ok(live) = state.get_mut()
        {
            live.clear();
        }
    }
}

/// Lowest level shown on the **plain** stderr path. `quiet` raises the bar to
/// `Warn`; otherwise `Info`. `Progress` is below both, so plain output never
/// shows transient ticks (those belong to the live block).
fn stderr_min_level(quiet: bool) -> BuildLogLevel {
    if quiet {
        BuildLogLevel::Warn
    } else {
        BuildLogLevel::Info
    }
}

/// Aggregate view of a run made of many small, interchangeable subjects.
///
/// A build's subjects are worth watching one by one: each is a long piece of
/// work with a name you recognize. A fetch run is not like that -- eight
/// hundred downloads, each uninteresting on its own, all started at once. A
/// line per subject there is worse than useless: the block grows past the
/// terminal, the tail is all you see, and redrawing it eats the scrollback, so
/// there is nothing above to scroll to either.
///
/// What is worth knowing instead is the shape of the whole: how much is left,
/// whether bytes are moving, which host everything is queued behind, and which
/// few downloads are taking unusually long. That fits in a fixed handful of
/// lines that never grow.
#[derive(Debug)]
struct FetchProgress {
    total: usize,
    done: usize,
    failed: usize,
    subjects: HashMap<String, SubjectPhase>,
    /// Bytes from downloads that have already finished; the ones still running
    /// are added from their live counters at render time.
    completed_bytes: u64,
    started: Instant,
    /// Bytes and the moment they were sampled, for a rate over a recent window
    /// rather than over the whole run -- an average since the start keeps
    /// reporting throughput long after a stall.
    rate_sample: (Instant, u64),
    rate_bytes_per_second: f64,
    /// Set once the queue has drained. The hosts line is about what is waiting,
    /// so it goes away when nothing is -- and stays away, because a retry
    /// briefly re-queues one download and the line must not blink back.
    hosts_line_retired: bool,
}

#[derive(Debug)]
enum SubjectPhase {
    /// Accepted, waiting for a connection slot. `host` is where it intends to
    /// go, known from the origin before the first request is made.
    Queued {
        host: String,
    },
    Active(ActiveDownload),
    /// A transient request failure is sleeping before its next attempt. This
    /// is neither network-slot waiting nor downloading, and deserves its own
    /// statistics counter so a stalled mirror is visible immediately.
    Retrying {
        host: String,
    },
}

#[derive(Debug)]
struct ActiveDownload {
    name: String,
    host: String,
    bytes: u64,
    total_bytes: Option<u64>,
    since: Instant,
}

/// How many long-running downloads the block names. Three is enough to show a
/// stall without pushing everything else off a short terminal.
const FETCH_SLOW_LINES: usize = 3;

impl FetchProgress {
    fn new(total: usize) -> Self {
        let now = Instant::now();
        Self {
            total,
            done: 0,
            failed: 0,
            subjects: HashMap::new(),
            completed_bytes: 0,
            started: now,
            rate_sample: (now, 0),
            rate_bytes_per_second: 0.0,
            hosts_line_retired: false,
        }
    }

    fn queued(&self) -> usize {
        self.subjects
            .values()
            .filter(|phase| matches!(phase, SubjectPhase::Queued { .. }))
            .count()
    }

    fn active(&self) -> usize {
        self.subjects
            .values()
            .filter(|phase| matches!(phase, SubjectPhase::Active(_)))
            .count()
    }

    fn retrying(&self) -> usize {
        self.subjects
            .values()
            .filter(|phase| matches!(phase, SubjectPhase::Retrying { .. }))
            .count()
    }

    fn live_bytes(&self) -> u64 {
        self.completed_bytes
            + self
                .subjects
                .values()
                .map(|phase| match phase {
                    SubjectPhase::Active(download) => download.bytes,
                    SubjectPhase::Queued { .. } | SubjectPhase::Retrying { .. } => 0,
                })
                .sum::<u64>()
    }

    /// Folds one event into the view. Returns whether the display should be
    /// redrawn -- everything that changes a number does, so the caller only has
    /// to decide how often to obey.
    fn handle(&mut self, record: &EventLogRecord) -> bool {
        let status = record.status.as_str();
        if status == BuildStatus::CacheHit.as_str() {
            self.done += 1;
            return true;
        }
        let Some(subject) = &record.subject else {
            // The fetcher reports what it skipped as a run-level `done` (a Path
            // source is nobody's subject), and it counts towards the total the
            // same way.
            if status == BuildStatus::Done.as_str() {
                self.done += 1;
                return true;
            }
            return false;
        };
        let key = subject.build_key.as_str();

        if let Some(host) = record.details.get("retry_host").and_then(Value::as_str) {
            if let Some(SubjectPhase::Active(download)) = self.subjects.remove(key) {
                self.completed_bytes += download.bytes;
            }
            self.subjects.insert(
                key.to_string(),
                SubjectPhase::Retrying {
                    host: host.to_string(),
                },
            );
            return true;
        }

        if status == BuildStatus::Done.as_str()
            || status == BuildStatus::Failed.as_str()
            || status == BuildStatus::Cancelled.as_str()
        {
            if let Some(SubjectPhase::Active(download)) = self.subjects.remove(key) {
                self.completed_bytes += download.bytes;
            }
            if status == BuildStatus::Failed.as_str() {
                self.failed += 1;
            } else if status == BuildStatus::Done.as_str() {
                self.done += 1;
            }
            return true;
        }

        // A download announces where it is going before it goes: `start` names
        // the host it intends to use, the first fetch milestone names the one it
        // actually reached (they differ once a mirror is taken).
        let host = record.details.get("host").and_then(Value::as_str);
        if status == BuildStatus::Start.as_str() {
            if let Some(host) = host {
                self.subjects.insert(
                    key.to_string(),
                    SubjectPhase::Queued {
                        host: host.to_string(),
                    },
                );
                return true;
            }
            return false;
        }

        let Some(host) = host else {
            // Some other running event (an import milestone, say): the counters
            // do not move.
            return false;
        };
        let bytes = record.details.get("bytes").and_then(Value::as_u64);
        let total_bytes = record.details.get("total_bytes").and_then(Value::as_u64);
        match self.subjects.get_mut(key) {
            Some(SubjectPhase::Active(download)) => {
                download.host = host.to_string();
                if let Some(bytes) = bytes {
                    download.bytes = bytes;
                }
                if total_bytes.is_some() {
                    download.total_bytes = total_bytes;
                }
            }
            _ => {
                self.subjects.insert(
                    key.to_string(),
                    SubjectPhase::Active(ActiveDownload {
                        name: subject.name.clone(),
                        host: host.to_string(),
                        bytes: bytes.unwrap_or(0),
                        total_bytes,
                        since: Instant::now(),
                    }),
                );
            }
        }
        true
    }

    /// Recomputes the rate from the bytes seen since the last sample, keeping
    /// the window long enough that a second without a chunk does not read as a
    /// stall.
    fn refresh_rate(&mut self, now: Instant) {
        const WINDOW: Duration = Duration::from_secs(3);
        let elapsed = now.saturating_duration_since(self.rate_sample.0);
        if elapsed < WINDOW {
            return;
        }
        let bytes = self.live_bytes();
        let gained = bytes.saturating_sub(self.rate_sample.1);
        self.rate_bytes_per_second = gained as f64 / elapsed.as_secs_f64();
        self.rate_sample = (now, bytes);
    }

    /// The whole block, one string per line.
    fn render(&mut self) -> Vec<String> {
        let width = terminal_width().unwrap_or(FETCH_ASSUMED_WIDTH);
        let now = Instant::now();
        self.refresh_rate(now);
        let queued = self.queued();
        if queued == 0 && !self.subjects.is_empty() {
            self.hosts_line_retired = true;
        }

        let mut lines = vec![
            format!(
                "fetch  {}/{} done · {} active · {} queued · {} failed          {}",
                self.done,
                self.total,
                self.active(),
                queued,
                self.failed,
                format_duration(now.saturating_duration_since(self.started)),
            ),
            format!(
                "       {} · {}/s",
                format_bytes(self.live_bytes()),
                format_bytes(self.rate_bytes_per_second as u64),
            ),
        ];
        if !self.hosts_line_retired
            && let Some(hosts) = self.render_hosts(width)
        {
            lines.push(hosts);
        }
        lines.extend(self.render_slow(now, width));
        lines
    }

    /// Hosts, worst backlog first: `ftp.gnu.org 6 (12 queued)` says the host's
    /// slots are full and twelve downloads are behind it.
    ///
    /// The count is spelled out rather than written as a fraction: `6/12`
    /// promises a part of a whole and these are two independent numbers, so it
    /// reads as nonsense in either order. A host with nothing waiting shows
    /// only its running count -- the parenthesis is the news, and it should be
    /// what catches the eye.
    fn render_hosts(&self, width: usize) -> Option<String> {
        let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for phase in self.subjects.values() {
            let (host, active) = match phase {
                SubjectPhase::Queued { host } => (host.as_str(), false),
                SubjectPhase::Active(download) => (download.host.as_str(), true),
                SubjectPhase::Retrying { host } => (host.as_str(), false),
            };
            let entry = counts.entry(host).or_insert((0, 0));
            if active {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
        if counts.is_empty() {
            return None;
        }
        // Sorted by what is waiting first: the line exists to name a
        // bottleneck, so the host holding one belongs at the front.
        let mut ordered: Vec<_> = counts.into_iter().collect();
        ordered.sort_by(|left, right| {
            let (left_host, (left_active, left_queued)) = left;
            let (right_host, (right_active, right_queued)) = right;
            right_queued
                .cmp(left_queued)
                .then(right_active.cmp(left_active))
                .then(left_host.cmp(right_host))
        });
        // As many as fit, never more than a few: the line is a pointer at the
        // worst offender, not an inventory. Room for the tail is reserved
        // before an entry is admitted, so "· 4 more" is never the thing that
        // gets cut in half.
        const MOST_SHOWN: usize = 4;
        let mut text = String::from("hosts  ");
        let mut shown = 0;
        for (host, (active, queued)) in ordered.iter().take(MOST_SHOWN) {
            let mut entry = String::new();
            if shown > 0 {
                entry.push_str(" · ");
            }
            entry.push_str(host);
            entry.push_str(&format!(" {active}"));
            if *queued > 0 {
                entry.push_str(&format!(" ({queued} queued)"));
            }
            let remaining = ordered.len() - shown - 1;
            let tail = if remaining > 0 {
                format!(" · {remaining} more").chars().count()
            } else {
                0
            };
            if shown > 0 && text.chars().count() + entry.chars().count() + tail > width {
                break;
            }
            text.push_str(&entry);
            shown += 1;
        }
        if ordered.len() > shown {
            // Spelled out, so it cannot be mistaken for another host's queue.
            text.push_str(&format!(" · {} more", ordered.len() - shown));
        }
        Some(text)
    }

    /// The longest-running downloads, in fixed columns.
    ///
    /// Sorted by age rather than by rate: age is monotonic, so a stalled
    /// download rises to the top and stays there instead of flickering as a
    /// second-by-second rate does. The byte counter beside it is what
    /// separates "a big file, arriving" from "stuck".
    ///
    /// The columns are constant widths so the eye can read down them. The name
    /// is truncated because it is the only unbounded field; the host is last
    /// and takes what is left, so a terminal too narrow for the line loses the
    /// host rather than the identity of what is slow.
    fn render_slow(&self, now: Instant, width: usize) -> Vec<String> {
        let mut active: Vec<&ActiveDownload> = self
            .subjects
            .values()
            .filter_map(|phase| match phase {
                SubjectPhase::Active(download) => Some(download),
                SubjectPhase::Queued { .. } | SubjectPhase::Retrying { .. } => None,
            })
            .collect();
        active.sort_by_key(|download| download.since);
        active
            .iter()
            .take(FETCH_SLOW_LINES)
            .enumerate()
            .map(|(index, download)| {
                let label = if index == 0 { "slow " } else { "     " };
                let line = format!(
                    "{label}  {:<name$}  {:>size$}  {:>time$}   ",
                    truncate(&download.name, FETCH_NAME_WIDTH),
                    format_progress_size(download.bytes, download.total_bytes),
                    format_duration(now.saturating_duration_since(download.since)),
                    name = FETCH_NAME_WIDTH,
                    size = FETCH_SIZE_WIDTH,
                    time = FETCH_TIME_WIDTH,
                );
                let room = width.saturating_sub(line.chars().count());
                format!("{line}{}", truncate(&download.host, room))
            })
            .collect()
    }
}

/// The stderr terminal's width, when stderr is one.
///
/// The block's last column is meant to take whatever room is left, and "what
/// is left" is a question only the terminal can answer. One ioctl per redraw
/// is nothing, and asking every time is also what keeps the layout right after
/// the window is resized mid-run.
fn terminal_size() -> Option<(usize, usize)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes the struct we hand it and nothing else; a
    // failure (stderr is not a terminal) leaves it untouched and is reported by
    // the return value.
    let rc = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) };
    if rc == 0 && size.ws_row > 0 && size.ws_col > 0 {
        Some((size.ws_row as usize, size.ws_col as usize))
    } else {
        None
    }
}

fn terminal_width() -> Option<usize> {
    terminal_size().map(|(_, columns)| columns)
}

fn terminal_height() -> Option<usize> {
    terminal_size().map(|(rows, _)| rows)
}

fn progress_line_budget(policy: ProgressPolicy, rows: usize) -> usize {
    match policy {
        // The live block always owns fetch statistics, builder statistics, and
        // the run status. Summary simply gives the activity viewport zero
        // rows; it is not a different, dynamically shaped block.
        ProgressPolicy::Summary => 3,
        ProgressPolicy::Fixed { max_lines } => max_lines.min(rows.saturating_sub(2).max(3)),
        ProgressPolicy::Auto => {
            let screen = rows.saturating_sub(2).max(3);
            let fraction = rows.saturating_mul(3) / 4;
            fraction.max(MIN_FIXED_PROGRESS_LINES).min(screen)
        }
    }
}

/// Width to lay the block out for when stderr is not a terminal (tests, and
/// the plain path's heartbeat).
const FETCH_ASSUMED_WIDTH: usize = 100;

/// Column widths of the `slow` lines. Size and time are set to their worst
/// cases -- `1024.0/1024.0 MB` (a value a byte below the next unit still
/// rounds up to four digits) and `h:mm:ss` -- so those columns never shift.
const FETCH_NAME_WIDTH: usize = 30;
const FETCH_SIZE_WIDTH: usize = 16;
const FETCH_TIME_WIDTH: usize = 7;

/// Shortens `text` to `width`, ending in `..` so a cut is visible as a cut.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(2)).collect();
    format!("{kept}..")
}

/// How much of a download has arrived: `1.2/1.9 GB` when the server declared a
/// length, plain bytes when it did not. One unit for both halves -- repeating
/// it (`1.2 GB/1.9 GB`) is longer and no clearer.
fn format_progress_size(bytes: u64, total: Option<u64>) -> String {
    let Some(total) = total else {
        return format_bytes(bytes);
    };
    let (scale, suffix) = byte_scale(total);
    format!(
        "{:.1}/{:.1} {suffix}",
        bytes as f64 / scale,
        total as f64 / scale
    )
}

/// The divisor and suffix of the largest unit that keeps `bytes` above one.
fn byte_scale(bytes: u64) -> (f64, &'static str) {
    const UNIT: f64 = 1024.0;
    let bytes = bytes as f64;
    for (scale, suffix) in [
        (UNIT * UNIT * UNIT * UNIT, "TB"),
        (UNIT * UNIT * UNIT, "GB"),
        (UNIT * UNIT, "MB"),
        (UNIT, "KB"),
    ] {
        if bytes >= scale {
            return (scale, suffix);
        }
    }
    (1.0, "B")
}

/// Bytes in the largest unit that keeps the number readable.
fn format_bytes(bytes: u64) -> String {
    let (scale, suffix) = byte_scale(bytes);
    if suffix == "B" {
        return format!("{bytes} B");
    }
    format!("{:.1} {suffix}", bytes as f64 / scale)
}

/// `m:ss` up to an hour, `h:mm:ss` past it.
fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 3600 {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}

/// Identity of one concrete activity shown in the terminal viewport.
///
/// A Source and a builder may theoretically have the same build key. Keeping
/// their kinds in the key prevents that coincidence from merging two unrelated
/// rows or letting a terminal event free the wrong row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ActivityKey {
    Builder(String),
    Source(String),
}

impl ActivityKey {
    fn is_builder(&self) -> bool {
        matches!(self, Self::Builder(_))
    }
}

#[derive(Debug)]
struct ActiveActivity {
    message: String,
    started_at: Instant,
    failed: bool,
}

#[derive(Debug, Default)]
struct ActivityViewport {
    active: HashMap<ActivityKey, ActiveActivity>,
    visible: Vec<Option<ActivityKey>>,
    hidden_builders: VecDeque<ActivityKey>,
    hidden_sources: VecDeque<ActivityKey>,
    capacity: usize,
}

impl ActivityViewport {
    fn start_or_update(&mut self, key: ActivityKey, message: String) {
        if let Some(activity) = self.active.get_mut(&key) {
            activity.message = message;
            return;
        }
        self.active.insert(
            key.clone(),
            ActiveActivity {
                message,
                started_at: Instant::now(),
                failed: false,
            },
        );
        if let Some(slot) = self.visible.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(key);
        } else {
            self.push_hidden(key);
        }
    }

    fn finish(&mut self, key: &ActivityKey) {
        self.active.remove(key);
        if let Some(index) = self
            .visible
            .iter()
            .position(|slot| slot.as_ref() == Some(key))
        {
            self.visible[index] = self.pop_hidden();
        } else {
            self.hidden_builders.retain(|candidate| candidate != key);
            self.hidden_sources.retain(|candidate| candidate != key);
        }
    }

    /// Keeps a visible failure in place until the run ends. A failure that was
    /// already hidden has no row to preserve, so it simply leaves the model.
    fn fail(&mut self, key: &ActivityKey, message: String) {
        let Some(activity) = self.active.get_mut(key) else {
            return;
        };
        activity.message = message;
        activity.failed = true;
        if !self.visible.iter().any(|slot| slot.as_ref() == Some(key)) {
            self.active.remove(key);
            self.hidden_builders.retain(|candidate| candidate != key);
            self.hidden_sources.retain(|candidate| candidate != key);
        }
    }

    fn set_capacity(&mut self, capacity: usize) {
        if self.capacity == capacity {
            return;
        }
        if capacity < self.visible.len() {
            let displaced = self.visible.split_off(capacity);
            // `push_front` in reverse preserves the visible order ahead of
            // activities that had already overflowed before the resize.
            for key in displaced.into_iter().flatten().rev() {
                self.push_hidden_front(key);
            }
        } else {
            self.visible.resize(capacity, None);
        }
        self.capacity = capacity;
        self.fill_idle_rows();
    }

    fn push_hidden(&mut self, key: ActivityKey) {
        if key.is_builder() {
            self.hidden_builders.push_back(key);
        } else {
            self.hidden_sources.push_back(key);
        }
    }

    fn push_hidden_front(&mut self, key: ActivityKey) {
        if key.is_builder() {
            self.hidden_builders.push_front(key);
        } else {
            self.hidden_sources.push_front(key);
        }
    }

    fn pop_hidden(&mut self) -> Option<ActivityKey> {
        for queue in [&mut self.hidden_builders, &mut self.hidden_sources] {
            while let Some(key) = queue.pop_front() {
                if self
                    .active
                    .get(&key)
                    .is_some_and(|activity| !activity.failed)
                {
                    return Some(key);
                }
            }
        }
        None
    }

    fn fill_idle_rows(&mut self) {
        for index in 0..self.visible.len() {
            if self.visible[index].is_none() {
                self.visible[index] = self.pop_hidden();
            }
        }
    }

    fn running_builders(&self) -> usize {
        self.active
            .iter()
            .filter(|(key, activity)| key.is_builder() && !activity.failed)
            .count()
    }

    fn hidden_builders(&self) -> usize {
        self.hidden_builders.len()
    }

    fn hidden_sources(&self) -> usize {
        self.hidden_sources.len()
    }
}

/// One terminal line in the live block. Bars are merely a viewport over the
/// full active-subject model and can be rebound after overflow or resize.
struct Slot {
    bar: ProgressBar,
    activity: Option<ActivityKey>,
}

/// Live indicatif state with two fixed statistics rows, a fixed viewport of
/// concrete activities, and a fixed bottom run summary.
struct LiveProgress {
    run_log_dir: PathBuf,
    multi: MultiProgress,
    fetch: ProgressBar,
    build: ProgressBar,
    summary: ProgressBar,
    active_style: ProgressStyle,
    failed_style: ProgressStyle,
    idle_style: ProgressStyle,
    slots: Vec<Slot>,
    viewport: ActivityViewport,
    policy: ProgressPolicy,
    rows_override: Option<usize>,
    reachable: usize,
    reachable_sources: usize,
    builder_done: usize,
    builder_cache_hits: usize,
    builder_failed: usize,
    queued_builders: HashSet<String>,
    fetch_progress: FetchProgress,
    last_drawn: Instant,
}

/// Source progress ticks can arrive by the dozen per second. The activity row
/// and statistics do not need a redraw for every byte counter update.
const LIVE_REDRAW: Duration = Duration::from_millis(200);

impl LiveProgress {
    fn new(run_log_dir: PathBuf, multi: MultiProgress, policy: ProgressPolicy) -> Self {
        let fetch = multi.add(ProgressBar::new_spinner());
        fetch.set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
        let build = multi.add(ProgressBar::new_spinner());
        build.set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
        let summary = multi.add(ProgressBar::new_spinner());
        summary.set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
        let now = Instant::now();
        Self {
            run_log_dir,
            multi,
            fetch,
            build,
            summary,
            active_style: ProgressStyle::with_template("{spinner} {msg} ({elapsed})")
                .expect("valid template"),
            failed_style: ProgressStyle::with_template("  {msg}").expect("valid template"),
            idle_style: ProgressStyle::with_template("  {msg}").expect("valid template"),
            slots: Vec::new(),
            viewport: ActivityViewport::default(),
            policy,
            rows_override: None,
            reachable: 0,
            reachable_sources: 0,
            builder_done: 0,
            builder_cache_hits: 0,
            builder_failed: 0,
            queued_builders: HashSet::new(),
            fetch_progress: FetchProgress::new(0),
            last_drawn: now - LIVE_REDRAW,
        }
    }

    fn running(&self) -> usize {
        self.viewport.running_builders()
    }

    fn update_headers(&mut self) {
        self.fetch_progress.refresh_rate(Instant::now());
        self.fetch.set_message(format!(
            "fetch: {} downloading · {} waiting · {} complete · {} retrying · {} failed",
            self.fetch_progress.active(),
            self.fetch_progress.queued(),
            self.fetch_progress.done,
            self.fetch_progress.retrying(),
            self.fetch_progress.failed,
        ));
        self.build.set_message(format!(
            "build: {} running · {} waiting · {} complete · {} failed",
            self.running(),
            self.queued_builders.len(),
            self.builder_done + self.builder_cache_hits,
            self.builder_failed,
        ));
        self.summary.set_message(format_run_progress(&RunProgress {
            built: self.builder_done,
            cache_hits: self.builder_cache_hits,
            build_failed: self.builder_failed,
            fetched: self.fetch_progress.done,
            fetch_failed: self.fetch_progress.failed,
            reachable: self.reachable,
            hidden_builders: self.viewport.hidden_builders(),
            hidden_sources: self.viewport.hidden_sources(),
        }));
    }

    fn start_or_update_activity(&mut self, key: ActivityKey, message: String) {
        self.viewport.start_or_update(key, message);
    }

    fn finish_activity(&mut self, key: &ActivityKey) {
        self.viewport.finish(key);
    }

    fn clear(&mut self) {
        for slot in self.slots.drain(..) {
            slot.bar.finish_and_clear();
        }
        self.viewport = ActivityViewport::default();
        self.fetch.finish_and_clear();
        self.build.finish_and_clear();
        self.summary.finish_and_clear();
    }

    fn current_rows(&self) -> usize {
        self.rows_override.or_else(terminal_height).unwrap_or(24)
    }

    fn refresh_layout(&mut self) {
        self.reflow(self.current_rows());
    }

    #[cfg(test)]
    fn reflow_for_test(&mut self, rows: usize) {
        self.rows_override = Some(rows);
        self.reflow(rows);
    }

    fn reflow(&mut self, rows: usize) {
        let budget = progress_line_budget(self.policy, rows);
        let capacity = if matches!(self.policy, ProgressPolicy::Summary) {
            0
        } else {
            budget.saturating_sub(3)
        };
        self.viewport.set_capacity(capacity);
        self.sync_activity_bars();
        self.update_headers();
    }

    fn sync_activity_bars(&mut self) {
        while self.slots.len() < self.viewport.visible.len() {
            let bar = self
                .multi
                .insert_before(&self.summary, ProgressBar::new_spinner());
            self.slots.push(Slot {
                bar,
                activity: None,
            });
        }
        while self.slots.len() > self.viewport.visible.len() {
            self.slots
                .pop()
                .expect("slot count was checked")
                .bar
                .finish_and_clear();
        }
        for (index, key) in self.viewport.visible.iter().enumerate() {
            let slot = &mut self.slots[index];
            match key {
                Some(key) => {
                    let activity = &self.viewport.active[key];
                    if activity.failed {
                        slot.bar.disable_steady_tick();
                        slot.bar.set_style(self.failed_style.clone());
                    } else if slot.activity.as_ref() != Some(key) {
                        slot.bar.set_style(self.active_style.clone());
                        slot.bar.set_elapsed(activity.started_at.elapsed());
                        slot.bar.enable_steady_tick(Duration::from_millis(120));
                    }
                    slot.activity = Some(key.clone());
                    slot.bar.set_message(activity.message.clone());
                }
                None => {
                    slot.bar.disable_steady_tick();
                    slot.bar.set_style(self.idle_style.clone());
                    slot.bar.set_message("—");
                    slot.activity = None;
                }
            }
        }
    }

    fn handle(&mut self, record: &EventLogRecord) {
        let status = record.status.as_str();

        if status == BuildStatus::RunStarted.as_str() {
            self.reachable = record
                .details
                .get("reachable")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| detail_u64(record, "subjects"))
                as usize;
            self.reachable_sources = detail_u64(record, "reachable_sources") as usize;
            self.fetch_progress = FetchProgress::new(self.reachable_sources);
            self.queued_builders.clear();
            self.reflow(self.current_rows());
            return;
        }
        if status == BuildStatus::RunFinished.as_str() {
            self.clear();
            // A build's totals live in its details; render them as before. Any
            // other run (the fetcher's, say) composes its own summary message,
            // and inventing zero "built" counters for it would be a lie.
            let line = if record.details.contains_key("built") {
                format!("done: {}", format_outcome_details(record))
            } else {
                format!("done: {}", record.message)
            };
            let _ = self.multi.println(line);
            return;
        }

        let network = record.details.get("transfer").and_then(Value::as_str) == Some("network");
        let builder_queue_event = record.subject.as_ref().is_some_and(|subject| {
            subject.tag != "Source"
                && subject.tag != "SecondaryContent"
                && status == BuildStatus::CacheMiss.as_str()
                && record
                    .details
                    .get("queued_for_builder")
                    .and_then(Value::as_bool)
                    == Some(true)
        });
        if builder_queue_event {
            let subject = record.subject.as_ref().expect("queue event has a subject");
            self.queued_builders.insert(subject.build_key.clone());
            self.reflow(self.current_rows());
            return;
        }
        if status == BuildStatus::Start.as_str()
            && let Some(subject) = &record.subject
            && subject.tag != "Source"
            && subject.tag != "SecondaryContent"
        {
            self.queued_builders.remove(&subject.build_key);
        }
        let source_terminal = record.subject.as_ref().is_some_and(|subject| {
            (subject.tag == "Source" || subject.tag == "SecondaryContent")
                && matches!(
                    status,
                    value if value == BuildStatus::CacheHit.as_str()
                        || value == BuildStatus::Done.as_str()
                        || value == BuildStatus::Failed.as_str()
                        || value == BuildStatus::Cancelled.as_str()
                )
        });
        let tracked_source = record.subject.as_ref().is_some_and(|subject| {
            self.fetch_progress
                .subjects
                .contains_key(&subject.build_key)
        });
        // A network Source `start` announces its intended host before it has a
        // connection permit. It belongs in fetch statistics as `waiting`, not
        // in an activity row. The first `running` network milestone is the
        // point at which the row becomes a useful description of real work.
        let source_transfer_started = network && status == BuildStatus::Running.as_str();
        let source_retry = record.details.contains_key("retry_host");
        let source_was_visible = record.subject.as_ref().is_some_and(|subject| {
            self.viewport
                .active
                .contains_key(&ActivityKey::Source(subject.build_key.clone()))
        });
        if network || tracked_source || source_terminal {
            if record.level >= BuildLogLevel::Warn {
                let _ = self
                    .multi
                    .println(format_progress_line(record, &self.run_log_dir));
            }
            let changed = self.fetch_progress.handle(record);
            if let Some(subject) = &record.subject {
                let key = ActivityKey::Source(subject.build_key.clone());
                if status == BuildStatus::Failed.as_str() {
                    self.viewport
                        .fail(&key, format_progress_line(record, &self.run_log_dir));
                } else if source_terminal {
                    self.finish_activity(&key);
                } else if source_transfer_started
                    || (source_retry && self.viewport.active.contains_key(&key))
                {
                    self.start_or_update_activity(
                        key,
                        format_progress_line(record, &self.run_log_dir),
                    );
                }
            }
            let now = Instant::now();
            let redraw = changed
                && (record.level != BuildLogLevel::Progress
                    || (source_transfer_started && !source_was_visible)
                    || now.duration_since(self.last_drawn) >= LIVE_REDRAW);
            if redraw {
                self.last_drawn = now;
                self.reflow(self.current_rows());
            }
            return;
        }
        if status == BuildStatus::CacheHit.as_str() {
            if let Some(subject) = &record.subject {
                self.queued_builders.remove(&subject.build_key);
            }
            self.builder_cache_hits += 1;
            self.reflow(self.current_rows());
            return;
        }

        let Some(subject) = &record.subject else {
            // Run-level non-terminal event with no subject: surface warnings and
            // errors above the block; ignore routine info in the live UI.
            if record.level >= BuildLogLevel::Warn {
                let _ = self
                    .multi
                    .println(format_progress_line(record, &self.run_log_dir));
            }
            return;
        };

        if subject.tag == "Source" || subject.tag == "SecondaryContent" {
            if record.level >= BuildLogLevel::Warn {
                let _ = self
                    .multi
                    .println(format_progress_line(record, &self.run_log_dir));
            }
            return;
        }

        if status == BuildStatus::Done.as_str() {
            self.queued_builders.remove(&subject.build_key);
            self.finish_activity(&ActivityKey::Builder(subject.build_key.clone()));
            self.builder_done += 1;
            self.reflow(self.current_rows());
            return;
        }
        if status == BuildStatus::Failed.as_str() {
            self.queued_builders.remove(&subject.build_key);
            // The warning is durable above the block, while a visible activity
            // row remains pinned until run-finished for immediate context.
            let _ = self
                .multi
                .println(format_progress_line(record, &self.run_log_dir));
            self.viewport.fail(
                &ActivityKey::Builder(subject.build_key.clone()),
                format_progress_line(record, &self.run_log_dir),
            );
            self.builder_failed += 1;
            self.reflow(self.current_rows());
            return;
        }
        if status == BuildStatus::Cancelled.as_str() {
            self.queued_builders.remove(&subject.build_key);
            self.finish_activity(&ActivityKey::Builder(subject.build_key.clone()));
            self.reflow(self.current_rows());
            return;
        }
        if record.level >= BuildLogLevel::Warn {
            // A non-terminal warning/error from a running subject: print above,
            // but keep its slot.
            let _ = self
                .multi
                .println(format_progress_line(record, &self.run_log_dir));
            return;
        }

        // start / running / progress: route to the subject's (possibly reused)
        // slot and update its line in place.
        let message = format_progress_line(record, &self.run_log_dir);
        self.start_or_update_activity(ActivityKey::Builder(subject.build_key.clone()), message);
        self.reflow(self.current_rows());
    }
}

struct RunProgress {
    built: usize,
    cache_hits: usize,
    build_failed: usize,
    fetched: usize,
    fetch_failed: usize,
    reachable: usize,
    hidden_builders: usize,
    hidden_sources: usize,
}

fn format_run_progress(progress: &RunProgress) -> String {
    let mut line = format!(
        "{} built · {} cache-hit · {} fetched · {} failed · {} reachable",
        progress.built,
        progress.cache_hits,
        progress.fetched,
        progress.build_failed + progress.fetch_failed,
        progress.reachable,
    );
    if progress.hidden_builders > 0 {
        line.push_str(&format!(" · {} builders hidden", progress.hidden_builders));
    }
    if progress.hidden_sources > 0 {
        line.push_str(&format!(" · {} downloads hidden", progress.hidden_sources));
    }
    line
}

impl EventSink for ProgressSink {
    fn write_event(&self, record: &EventLogRecord) {
        match self {
            Self::Live(state) => {
                if let Ok(mut live) = state.lock() {
                    live.handle(record);
                }
            }
            Self::Plain {
                run_log_dir,
                min_level,
                aggregate,
            } => {
                let Ok(mut aggregate) = aggregate.lock() else {
                    return;
                };
                let status = record.status.as_str();
                if status == BuildStatus::RunStarted.as_str()
                    && record.details.get("progress").and_then(Value::as_str) == Some("aggregate")
                {
                    *aggregate = Some(PlainAggregate {
                        progress: FetchProgress::new(detail_u64(record, "sources") as usize),
                        last_printed: Instant::now(),
                    });
                }
                if let Some(state) = aggregate.as_mut()
                    && status != BuildStatus::RunFinished.as_str()
                {
                    state.progress.handle(record);
                    // Routine per-subject chatter is what the heartbeat
                    // replaces; anything at warning or above still speaks for
                    // itself, at once.
                    if record.level >= BuildLogLevel::Warn {
                        eprintln!("{}", format_progress_line(record, run_log_dir));
                    } else if *min_level <= BuildLogLevel::Info
                        && Instant::now().duration_since(state.last_printed)
                            >= PLAIN_AGGREGATE_HEARTBEAT
                    {
                        state.last_printed = Instant::now();
                        for line in state.progress.render().iter().take(2) {
                            eprintln!("{line}");
                        }
                    }
                    return;
                }
                if record.level >= *min_level {
                    eprintln!("{}", format_progress_line(record, run_log_dir));
                }
            }
        }
    }
}

/// A fully assembled, envelope-stamped event record: the serialized form
/// written to `events.jsonl` and handed to each sink.
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

/// One numeric field of an event's `details`, defaulting to 0 when absent.
fn detail_u64(record: &EventLogRecord, key: &str) -> u64 {
    record.details.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn format_progress_line(record: &EventLogRecord, run_log_dir: &Path) -> String {
    let mut line = if let Some(subject) = &record.subject {
        // Subject lines lead with the builder/source tag and recipe name; the
        // build key lives in the logs, not on screen. The realized object hash
        // is kept when present (it is the result identity, shown on completion).
        let mut line = format!("{} {}", subject.tag, subject.name);
        if let Some(object_hash) = &subject.object_hash {
            line.push(' ');
            line.push_str(&short_id(object_hash));
        }
        line
    } else {
        // Run-level lines have no subject, so the status/op label is the only
        // structure.
        let label = record.op.as_deref().unwrap_or(record.status.as_str());
        format!("[{label}]")
    };

    if !record.message.is_empty() {
        line.push_str(": ");
        line.push_str(&record.message);
    }

    // The run's totals live in the event details, and until now only the live
    // progress block rendered them. Off a terminal -- CI logs, the
    // rebuild-world log, the MCP build server -- that left the run ending on
    // "build finished" with no outcome at all.
    if record.status == BuildStatus::RunFinished.as_str() && record.details.contains_key("built") {
        line.push_str(&format!("; {}", format_outcome_details(record)));
    }

    if let Some(raw_log) = &record.raw_log {
        line.push_str(" (log: ");
        line.push_str(&run_log_dir.join(raw_log).display().to_string());
        line.push(')');
    }

    line
}

fn format_outcome_details(record: &EventLogRecord) -> String {
    let mut parts = vec![
        format!("{} built", detail_u64(record, "built")),
        format!("{} cache-hit", detail_u64(record, "cache_hit")),
    ];
    if record.details.contains_key("downloaded") {
        parts.extend([
            format!("{} downloaded", detail_u64(record, "downloaded")),
            format!("{} local", detail_u64(record, "local")),
            format!("{} secondary", detail_u64(record, "secondary")),
            format!("{} already-present", detail_u64(record, "already_present")),
        ]);
    }
    parts.push(format!("{} failed", detail_u64(record, "failed")));
    let cancelled = detail_u64(record, "cancelled");
    if cancelled > 0 {
        parts.push(format!("{cancelled} cancelled"));
    }
    parts.join(" · ")
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
    use serde_json::{Value, json};
    use std::fs;
    use tempfile::tempdir;

    fn run_event_log(run_log_dir: &Path) -> String {
        fs::read_to_string(run_log_dir.join("events.jsonl")).unwrap()
    }

    /// Helper: a log subject with a build key derived from `index`.
    fn test_subject(run_log_dir: &Path, index: usize) -> BuildLogSubject {
        let subject_dir = run_log_dir.join(format!("{index:08}-Sandbox-node"));
        BuildLogSubject::new(
            "Sandbox",
            "node",
            format!("{index:064}"),
            subject_dir.clone(),
            subject_dir.join("raw"),
        )
    }

    fn info_event(message: &str) -> BuildLogEvent {
        BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::Start,
            op: None,
            message: message.to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        }
    }

    #[test]
    fn releasing_a_subject_closes_its_log_file() {
        // The leak this guards against: one open events.jsonl per subject, held
        // for the whole run, exhausting the process descriptor limit part-way
        // through a large build. Only live subjects may hold a writer.
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        fs::create_dir_all(&run_log_dir).unwrap();
        let sink = FileSink::new(&run_log_dir).unwrap();

        for index in 0..50 {
            let subject = test_subject(&run_log_dir, index);
            sink.register_subject(&subject).unwrap();
            sink.release_subject(subject.identity());
        }
        assert_eq!(sink.subject_writers.lock().unwrap().len(), 0);

        // A registered-but-not-released subject keeps exactly its own writer.
        let live = test_subject(&run_log_dir, 100);
        sink.register_subject(&live).unwrap();
        assert_eq!(sink.subject_writers.lock().unwrap().len(), 1);
        sink.release_subject(live.identity());
        assert_eq!(sink.subject_writers.lock().unwrap().len(), 0);
    }

    #[test]
    fn a_released_subject_is_not_reopened_by_a_late_event() {
        // Cache-hit events carry a subject identity but no bound logger. Such an
        // event must land in the run log only, never resurrect a subject writer.
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        fs::create_dir_all(&run_log_dir).unwrap();
        let sink = FileSink::new(&run_log_dir).unwrap();

        let subject = test_subject(&run_log_dir, 3);
        sink.register_subject(&subject).unwrap();
        sink.release_subject(subject.identity());

        let record = EventLogRecord::assemble(
            0,
            Some(0),
            Some(subject.identity()),
            &info_event("late"),
            &run_log_dir,
        );
        sink.write_event(&record);
        assert_eq!(sink.subject_writers.lock().unwrap().len(), 0);
    }

    #[test]
    fn dropping_a_bound_logger_flushes_and_closes_the_subject_log() {
        // End-to-end over the path the executor actually takes: it drops the
        // bound logger when a subject ends. Info events stay buffered and
        // nothing flushes a finished subject afterwards, so the drop must --
        // otherwise the tail of every subject log would survive only by luck.
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );

        let subject = test_subject(&run_log_dir, 7);
        let subject_dir = run_log_dir.join("00000007-Sandbox-node");
        let node_logger = logger.bind_subject(subject).unwrap();
        node_logger.log_event(info_event("buffered info"));

        let event_log = subject_dir.join("events.jsonl");
        assert_eq!(fs::read_to_string(&event_log).unwrap(), "");

        drop(node_logger);
        let contents = fs::read_to_string(&event_log).unwrap();
        let event: Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(event["message"], Value::String("buffered info".to_string()));
    }

    #[test]
    fn quiet_stderr_threshold_drops_progress_but_keeps_warnings() {
        // quiet: only Warn/Error reach stderr; Info (progress) is dropped.
        let quiet = stderr_min_level(true);
        assert!(BuildLogLevel::Info < quiet);
        assert!(BuildLogLevel::Warn >= quiet);
        assert!(BuildLogLevel::Error >= quiet);

        // quiet also drops transient progress.
        assert!(BuildLogLevel::Progress < quiet);

        // normal (plain path): Info and up reach stderr. Transient Progress is
        // NOT shown on the plain path — it belongs to the live block only.
        let normal = stderr_min_level(false);
        assert!(BuildLogLevel::Progress < normal);
        assert!(BuildLogLevel::Info >= normal);
        assert!(BuildLogLevel::Warn >= normal);
        assert!(BuildLogLevel::Error >= normal);
    }

    fn live_subject_record(
        level: BuildLogLevel,
        status: BuildStatus,
        build_key: &str,
    ) -> EventLogRecord {
        let identity = SubjectIdentity::new("Tree", "pkg", build_key);
        EventLogRecord::assemble(
            0,
            Some(0),
            Some(&identity),
            &BuildLogEvent {
                level,
                status,
                op: None,
                message: "step".to_string(),
                object_hash: None,
                raw_log_path: None,
                details: Map::new(),
            },
            Path::new("/run"),
        )
    }

    fn live_run_record(status: BuildStatus, details: Value) -> EventLogRecord {
        EventLogRecord::assemble(
            0,
            None,
            None,
            &BuildLogEvent {
                level: BuildLogLevel::Info,
                status,
                op: None,
                message: "run".to_string(),
                object_hash: None,
                raw_log_path: None,
                details: details.as_object().cloned().unwrap_or_default(),
            },
            Path::new("/run"),
        )
    }

    fn queued_builder_record(build_key: &str) -> EventLogRecord {
        let identity = SubjectIdentity::new("Tree", "pkg", build_key);
        EventLogRecord::assemble(
            0,
            None,
            Some(&identity),
            &BuildLogEvent {
                level: BuildLogLevel::Progress,
                status: BuildStatus::CacheMiss,
                op: Some("queued".to_string()),
                message: "waiting for builder slot".to_string(),
                object_hash: None,
                raw_log_path: None,
                details: json!({ "queued_for_builder": true })
                    .as_object()
                    .unwrap()
                    .clone(),
            },
            Path::new("/run"),
        )
    }

    #[test]
    fn live_progress_keeps_idle_slots_and_reuses_them() {
        let sink = ProgressSink::live_hidden_with_policy(
            PathBuf::from("/run"),
            ProgressPolicy::Fixed { max_lines: 5 },
        );
        let bk = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
        let running = |level, status, key: &str| {
            sink.write_event(&live_subject_record(level, status, key));
        };

        sink.write_event(&live_run_record(
            BuildStatus::RunStarted,
            serde_json::json!({ "subjects": 4, "jobs": 2 }),
        ));
        running(BuildLogLevel::Info, BuildStatus::Running, &bk('a'));
        // A progress tick updates A's line in place (no new slot).
        running(BuildLogLevel::Progress, BuildStatus::Running, &bk('a'));
        running(BuildLogLevel::Info, BuildStatus::Running, &bk('b'));

        // A finishes: its slot stays (idle); the block does not shrink.
        running(BuildLogLevel::Info, BuildStatus::Done, &bk('a'));
        {
            let ProgressSink::Live(state) = &sink else {
                panic!("expected live sink");
            };
            let live = state.lock().unwrap();
            assert_eq!(live.slots.len(), 2, "block does not shrink");
            assert_eq!(live.running(), 1);
            assert_eq!(live.builder_done, 1);
        }

        // A new subject reuses A's idle slot instead of growing the block.
        running(BuildLogLevel::Info, BuildStatus::Running, &bk('d'));
        {
            let ProgressSink::Live(state) = &sink else {
                panic!("expected live sink");
            };
            let live = state.lock().unwrap();
            assert_eq!(live.slots.len(), 2, "idle slot reused, not grown");
            assert_eq!(live.running(), 2);
        }

        // cache-hit counts as done but occupies no slot; B and D finish.
        running(BuildLogLevel::Info, BuildStatus::CacheHit, &bk('e'));
        running(BuildLogLevel::Info, BuildStatus::Done, &bk('b'));
        running(BuildLogLevel::Error, BuildStatus::Failed, &bk('d'));

        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        let live = state.lock().unwrap();
        assert_eq!(live.slots.len(), 2, "two lines remain, now both idle");
        assert_eq!(live.running(), 0);
        assert_eq!(
            live.builder_done + live.builder_cache_hits,
            3,
            "A + B done, plus one cache-hit"
        );
        assert_eq!(live.builder_failed, 1);
        drop(live);

        running(BuildLogLevel::Info, BuildStatus::Start, &bk('f'));
        running(BuildLogLevel::Info, BuildStatus::Cancelled, &bk('f'));
        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        let live = state.lock().unwrap();
        assert_eq!(live.running(), 0, "cancelled subject releases its slot");
        assert_eq!(
            live.builder_done + live.builder_cache_hits,
            3,
            "cancellation is not successful work"
        );
        assert_eq!(
            live.builder_failed, 1,
            "cancellation is not a build failure"
        );
    }

    #[test]
    fn queued_builder_counts_as_waiting_until_its_worker_starts() {
        let sink = ProgressSink::live_hidden(PathBuf::from("/run"));
        let key = "q".repeat(64);
        sink.write_event(&live_run_record(
            BuildStatus::RunStarted,
            json!({ "reachable": 1, "reachable_sources": 0, "jobs": 1 }),
        ));
        sink.write_event(&queued_builder_record(&key));
        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        {
            let live = state.lock().unwrap();
            assert_eq!(live.queued_builders.len(), 1);
            assert_eq!(
                live.build.message(),
                "build: 0 running · 1 waiting · 0 complete · 0 failed"
            );
        }
        sink.write_event(&live_subject_record(
            BuildLogLevel::Info,
            BuildStatus::Start,
            &key,
        ));
        let live = state.lock().unwrap();
        assert!(live.queued_builders.is_empty());
        assert_eq!(live.running(), 1);
    }

    #[test]
    fn viewport_promotes_oldest_hidden_and_resize_keeps_oldest_visible() {
        let mut viewport = ActivityViewport::default();
        viewport.set_capacity(2);
        for key in ["a", "b", "c", "d"] {
            viewport.start_or_update(ActivityKey::Builder(key.into()), key.to_string());
        }
        assert_eq!(
            viewport.visible,
            [
                Some(ActivityKey::Builder("a".into())),
                Some(ActivityKey::Builder("b".into()))
            ]
        );
        assert_eq!(viewport.hidden_builders(), 2);

        viewport.finish(&ActivityKey::Builder("a".into()));
        assert_eq!(
            viewport.visible,
            [
                Some(ActivityKey::Builder("c".into())),
                Some(ActivityKey::Builder("b".into()))
            ]
        );
        assert_eq!(viewport.hidden_builders(), 1);

        viewport.set_capacity(1);
        assert_eq!(viewport.visible, [Some(ActivityKey::Builder("c".into()))]);
        assert_eq!(viewport.hidden_builders(), 2);

        viewport.set_capacity(3);
        assert_eq!(
            viewport.visible,
            [
                Some(ActivityKey::Builder("c".into())),
                Some(ActivityKey::Builder("b".into())),
                Some(ActivityKey::Builder("d".into()))
            ]
        );
        assert_eq!(viewport.hidden_builders(), 0);

        viewport.finish(&ActivityKey::Builder("d".into()));
        assert_eq!(
            viewport.visible,
            [
                Some(ActivityKey::Builder("c".into())),
                Some(ActivityKey::Builder("b".into())),
                None
            ]
        );
        viewport.set_capacity(1);
        assert_eq!(viewport.visible, [Some(ActivityKey::Builder("c".into()))]);
        viewport.set_capacity(3);
        assert_eq!(
            viewport.visible,
            [
                Some(ActivityKey::Builder("c".into())),
                Some(ActivityKey::Builder("b".into())),
                None
            ],
            "a temporary shrink must restore the trailing idle row"
        );
    }

    #[test]
    fn viewport_promotes_hidden_builders_before_hidden_sources() {
        let mut viewport = ActivityViewport::default();
        viewport.set_capacity(1);
        let builder_one = ActivityKey::Builder("builder-one".into());
        let source = ActivityKey::Source("source".into());
        let builder_two = ActivityKey::Builder("builder-two".into());
        viewport.start_or_update(builder_one.clone(), "builder one".into());
        viewport.start_or_update(source.clone(), "source".into());
        viewport.start_or_update(builder_two.clone(), "builder two".into());

        assert_eq!(viewport.hidden_builders(), 1);
        assert_eq!(viewport.hidden_sources(), 1);
        viewport.finish(&builder_one);
        assert_eq!(viewport.visible, [Some(builder_two.clone())]);
        viewport.finish(&builder_two);
        assert_eq!(viewport.visible, [Some(source)]);
    }

    #[test]
    fn visible_failure_stays_pinned_until_the_run_ends() {
        let mut viewport = ActivityViewport::default();
        viewport.set_capacity(1);
        let failed = ActivityKey::Builder("failed".into());
        viewport.start_or_update(failed.clone(), "compiling".into());
        viewport.fail(&failed, "compile failed".into());
        viewport.start_or_update(ActivityKey::Builder("next".into()), "next".into());

        assert_eq!(viewport.running_builders(), 1);
        assert_eq!(viewport.hidden_builders(), 1);
        assert_eq!(viewport.visible, [Some(failed)]);
    }

    #[test]
    fn progress_policy_caps_the_complete_builder_block() {
        let fixed_sink = ProgressSink::live_hidden_with_policy(
            PathBuf::from("/run"),
            ProgressPolicy::Fixed { max_lines: 8 },
        );
        let ProgressSink::Live(fixed) = &fixed_sink else {
            panic!("expected live sink");
        };
        {
            let mut live = fixed.lock().unwrap();
            live.reflow_for_test(24);
            for index in 0..10 {
                live.start_or_update_activity(
                    ActivityKey::Builder(format!("fixed-{index}")),
                    format!("fixed {index}"),
                );
            }
            live.reflow_for_test(24);
            assert_eq!(live.slots.len(), 5);
            assert_eq!(live.viewport.hidden_builders(), 5);
        }

        let summary_sink =
            ProgressSink::live_hidden_with_policy(PathBuf::from("/run"), ProgressPolicy::Summary);
        let ProgressSink::Live(summary) = &summary_sink else {
            panic!("expected live sink");
        };
        {
            let mut live = summary.lock().unwrap();
            live.reflow_for_test(24);
            for index in 0..3 {
                live.start_or_update_activity(
                    ActivityKey::Builder(format!("summary-{index}")),
                    format!("summary {index}"),
                );
            }
            assert!(live.slots.is_empty());
            assert_eq!(live.viewport.hidden_builders(), 3);
        }
    }

    #[test]
    fn auto_layout_shrinks_and_grows_without_losing_active_state() {
        let sink =
            ProgressSink::live_hidden_with_policy(PathBuf::from("/run"), ProgressPolicy::Auto);
        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        let mut live = state.lock().unwrap();
        live.reflow_for_test(24);
        for index in 0..20 {
            live.start_or_update_activity(
                ActivityKey::Builder(format!("job-{index}")),
                format!("job {index}"),
            );
        }
        assert_eq!(live.running(), 20);
        assert_eq!(live.slots.len(), 15);
        assert_eq!(live.viewport.hidden_builders(), 5);

        live.reflow_for_test(10);
        assert_eq!(live.running(), 20);
        assert_eq!(live.slots.len(), 4);
        assert_eq!(live.viewport.hidden_builders(), 16);

        live.reflow_for_test(40);
        assert_eq!(live.running(), 20);
        assert_eq!(live.slots.len(), 27);
        assert_eq!(live.viewport.hidden_builders(), 0);
    }

    #[test]
    fn build_progress_names_the_reachable_graph_without_a_fraction() {
        assert_eq!(
            format_run_progress(&RunProgress {
                built: 24,
                cache_hits: 19,
                build_failed: 0,
                fetched: 712,
                fetch_failed: 0,
                reachable: 1907,
                hidden_builders: 3,
                hidden_sources: 2,
            }),
            "24 built · 19 cache-hit · 712 fetched · 0 failed · 1907 reachable · 3 builders hidden · 2 downloads hidden"
        );
    }

    #[test]
    fn sink_uses_plain_path_off_tty() {
        // Skip under a real terminal (rare for `cargo test`, but be robust).
        if std::io::stderr().is_terminal() {
            return;
        }
        assert!(matches!(
            ProgressSink::new(PathBuf::from("/run"), false, ProgressPolicy::Auto),
            ProgressSink::Plain {
                min_level: BuildLogLevel::Info,
                ..
            }
        ));
        assert!(matches!(
            ProgressSink::new(PathBuf::from("/run"), true, ProgressPolicy::Auto),
            ProgressSink::Plain {
                min_level: BuildLogLevel::Warn,
                ..
            }
        ));
    }

    #[test]
    fn download_retries_are_counted_per_reason_too() {
        // Same events, second question: which of them is the machine's own
        // resolver and which is a host refusing. A count without that answers
        // neither.
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );

        let retry = |host: &str, reason: Option<&str>| {
            let mut details = Map::new();
            details.insert("retry_host".to_string(), Value::String(host.to_string()));
            if let Some(reason) = reason {
                details.insert(
                    "retry_reason".to_string(),
                    Value::String(reason.to_string()),
                );
            }
            BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::Running,
                op: Some("fetch".to_string()),
                message: "retrying".to_string(),
                object_hash: None,
                raw_log_path: None,
                details,
            }
        };
        logger.log_run_event(retry("a.example", Some("dns")));
        logger.log_run_event(retry("b.example", Some("dns")));
        logger.log_run_event(retry("b.example", Some("http 5xx")));
        // No reason at all: still a retry, still counted by host.
        logger.log_run_event(retry("c.example", None));

        assert_eq!(
            logger.download_retries(),
            BTreeMap::from([
                ("a.example".to_string(), 1),
                ("b.example".to_string(), 2),
                ("c.example".to_string(), 1),
            ])
        );
        assert_eq!(
            logger.download_retry_reasons(),
            BTreeMap::from([("dns".to_string(), 2), ("http 5xx".to_string(), 1)])
        );
    }

    #[test]
    fn download_retries_are_counted_per_host() {
        // Counted from the event's fields, not from its sentence: the message is
        // written for a person and may be reworded at any time.
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );
        assert!(logger.download_retries().is_empty());

        let retry = |host: &str| {
            let mut details = Map::new();
            details.insert("retry_host".to_string(), Value::String(host.to_string()));
            BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::Running,
                op: Some("fetch".to_string()),
                message: "retrying".to_string(),
                object_hash: None,
                raw_log_path: None,
                details,
            }
        };
        logger.log_run_event(retry("github.com"));
        logger.log_run_event(retry("static.crates.io"));
        logger.log_run_event(retry("github.com"));
        // An ordinary fetch milestone carries no host and must not be counted.
        logger.log_run_event(info_event("fetching something"));

        let counts = logger.download_retries();
        assert_eq!(counts.get("github.com"), Some(&2));
        assert_eq!(counts.get("static.crates.io"), Some(&1));
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn plain_run_finished_line_carries_the_run_totals() {
        // Off a terminal the live block never renders, so the counts have to be
        // in the line itself -- otherwise a CI log or a build server sees a run
        // end with "build finished" and no outcome.
        let mut details = Map::new();
        details.insert("built".to_string(), Value::from(12));
        details.insert("cache_hit".to_string(), Value::from(340));
        details.insert("failed".to_string(), Value::from(2));
        let event = BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::RunFinished,
            op: None,
            message: "build finished".to_string(),
            object_hash: None,
            raw_log_path: None,
            details,
        };
        let run_log_dir = PathBuf::from("/run");
        let record = EventLogRecord::assemble(7, None, None, &event, &run_log_dir);

        let line = format_progress_line(&record, &run_log_dir);
        assert_eq!(
            line,
            "[run-finished]: build finished; 12 built · 340 cache-hit · 2 failed"
        );
    }

    #[test]
    fn outcome_sink_counts_builder_and_source_categories_once() {
        let sink = OutcomeSink::default();
        sink.write_event(&live_subject_record(
            BuildLogLevel::Info,
            BuildStatus::Done,
            &"b".repeat(64),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Done,
            BuildLogLevel::Info,
            Some(("download", "download")),
            json!({ "source_outcome": "downloaded" }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::CacheHit,
            BuildLogLevel::Info,
            Some(("present", "present")),
            json!({ "source_outcome": "already_present" }),
        ));
        sink.write_event(&live_subject_record(
            BuildLogLevel::Error,
            BuildStatus::Failed,
            &"f".repeat(64),
        ));
        sink.write_event(&live_subject_record(
            BuildLogLevel::Info,
            BuildStatus::Cancelled,
            &"c".repeat(64),
        ));

        assert_eq!(
            sink.snapshot(),
            RunOutcomeStats {
                built: 1,
                cache_hit: 1,
                failed: 1,
                cancelled: 1,
                downloaded: 1,
                already_present: 1,
                ..RunOutcomeStats::default()
            }
        );
    }

    /// One event for the aggregate view, with whatever fields it carries.
    fn fetch_record(
        status: BuildStatus,
        level: BuildLogLevel,
        subject: Option<(&str, &str)>,
        details: Value,
    ) -> EventLogRecord {
        let Value::Object(details) = details else {
            panic!("details must be an object")
        };
        let event = BuildLogEvent {
            level,
            status,
            op: None,
            message: String::new(),
            object_hash: None,
            raw_log_path: None,
            details,
        };
        let subject = subject.map(|(name, key)| SubjectIdentity::new("Source", name, key));
        EventLogRecord::assemble(0, None, subject.as_ref(), &event, Path::new("/run"))
    }

    fn queued(name: &str, host: &str) -> EventLogRecord {
        fetch_record(
            BuildStatus::Start,
            BuildLogLevel::Info,
            Some((name, name)),
            json!({ "host": host }),
        )
    }

    fn downloading(name: &str, host: &str, bytes: u64) -> EventLogRecord {
        fetch_record(
            BuildStatus::Running,
            BuildLogLevel::Progress,
            Some((name, name)),
            json!({ "host": host, "bytes": bytes, "transfer": "network" }),
        )
    }

    #[test]
    fn the_aggregate_view_counts_queued_active_and_done() {
        // The number the per-subject block never showed is `queued`, and it is
        // the one that answers "how much is left" when eight hundred downloads
        // are waiting on a handful of connection slots.
        let mut progress = FetchProgress::new(3);
        progress.handle(&queued("a", "example.org"));
        progress.handle(&queued("b", "example.org"));
        progress.handle(&queued("c", "mirror.net"));
        assert_eq!((progress.queued(), progress.active()), (3, 0));

        progress.handle(&downloading("a", "example.org", 1024));
        assert_eq!((progress.queued(), progress.active()), (2, 1));

        progress.handle(&fetch_record(
            BuildStatus::Done,
            BuildLogLevel::Info,
            Some(("a", "a")),
            json!({}),
        ));
        assert_eq!(
            (progress.queued(), progress.active(), progress.done),
            (2, 0, 1)
        );
        // The finished download's bytes stay counted after it leaves.
        assert_eq!(progress.live_bytes(), 1024);
    }

    #[test]
    fn live_build_uses_one_fixed_viewport_for_builder_and_source_activity() {
        let sink =
            ProgressSink::live_hidden_with_policy(PathBuf::from("/run"), ProgressPolicy::Auto);
        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        state.lock().unwrap().reflow_for_test(24);
        sink.write_event(&live_run_record(
            BuildStatus::RunStarted,
            json!({ "reachable": 1907, "reachable_sources": 991, "jobs": 20 }),
        ));
        {
            let live = state.lock().unwrap();
            assert_eq!(live.fetch_progress.total, 991);
            assert_eq!(live.slots.len(), 15, "two headers, 15 rows, one run line");
            assert!(live.slots.iter().all(|slot| slot.activity.is_none()));
            assert_eq!(
                live.fetch.message(),
                "fetch: 0 downloading · 0 waiting · 0 complete · 0 retrying · 0 failed"
            );
            assert_eq!(
                live.build.message(),
                "build: 0 running · 0 waiting · 0 complete · 0 failed"
            );
        }
        sink.write_event(&live_subject_record(
            BuildLogLevel::Info,
            BuildStatus::Start,
            &"b".repeat(64),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Start,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({ "host": "example.org", "transfer": "network" }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::CacheMiss,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({}),
        ));

        {
            let live = state.lock().unwrap();
            assert_eq!(live.running(), 1, "Source is not a builder row");
            assert_eq!(live.slots.len(), 15);
            assert_eq!(live.fetch_progress.queued(), 1);
            assert!(live.slots[1].activity.is_none());
        }

        sink.write_event(&fetch_record(
            BuildStatus::Running,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({ "host": "example.org", "bytes": 1, "transfer": "network" }),
        ));
        {
            let live = state.lock().unwrap();
            assert_eq!(live.fetch_progress.active(), 1);
            assert_eq!(
                live.slots[1].activity,
                Some(ActivityKey::Source("source".to_string()))
            );
        }

        sink.write_event(&fetch_record(
            BuildStatus::Done,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({}),
        ));
        let live = state.lock().unwrap();
        assert_eq!(live.fetch_progress.done, 1);
        assert!(live.slots[1].activity.is_none());
        assert_eq!(live.running(), 1);
    }

    #[test]
    fn local_source_updates_fetch_statistics_without_taking_an_activity_row() {
        let sink = ProgressSink::live_hidden(PathBuf::from("/run"));
        sink.write_event(&live_run_record(
            BuildStatus::RunStarted,
            json!({ "reachable": 2, "reachable_sources": 1, "jobs": 1 }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Start,
            BuildLogLevel::Info,
            Some(("local", "local")),
            json!({ "host": "local", "transfer": "local" }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Done,
            BuildLogLevel::Info,
            Some(("local", "local")),
            json!({ "source_outcome": "local" }),
        ));
        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        let live = state.lock().unwrap();
        assert_eq!(live.fetch_progress.done, 1);
        assert_eq!(live.fetch_progress.active(), 0);
        assert_eq!(live.fetch_progress.queued(), 0);
        assert_eq!(live.running(), 0);
        assert!(live.slots.iter().all(|slot| slot.activity.is_none()));
    }

    #[test]
    fn retry_before_any_transfer_updates_statistics_without_taking_a_row() {
        let sink = ProgressSink::live_hidden(PathBuf::from("/run"));
        sink.write_event(&live_run_record(
            BuildStatus::RunStarted,
            json!({ "reachable": 1, "reachable_sources": 1, "jobs": 1 }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Start,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({ "host": "example.org", "transfer": "network" }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Running,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({ "retry_host": "example.org" }),
        ));
        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        let live = state.lock().unwrap();
        assert_eq!(live.fetch_progress.retrying(), 1);
        assert!(live.slots.iter().all(|slot| slot.activity.is_none()));
    }

    #[test]
    fn source_progress_ticks_update_counters_without_redrawing_every_tick() {
        let sink = ProgressSink::live_hidden(PathBuf::from("/run"));
        sink.write_event(&live_run_record(
            BuildStatus::RunStarted,
            json!({ "reachable": 1, "reachable_sources": 1, "jobs": 1 }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Start,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({ "host": "example.org", "transfer": "network" }),
        ));
        sink.write_event(&fetch_record(
            BuildStatus::Running,
            BuildLogLevel::Info,
            Some(("source", "source")),
            json!({ "host": "example.org", "bytes": 0, "transfer": "network" }),
        ));
        {
            let ProgressSink::Live(state) = &sink else {
                panic!("expected live sink");
            };
            state.lock().unwrap().last_drawn = Instant::now();
        }
        let mut first = downloading("source", "example.org", 1);
        first.message = "first byte".to_string();
        sink.write_event(&first);
        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        {
            let live = state.lock().unwrap();
            assert_eq!(live.fetch_progress.live_bytes(), 1);
            assert!(
                !live.slots[0].bar.message().contains("first byte"),
                "a fresh progress tick must not redraw the row"
            );
        }
        state.lock().unwrap().last_drawn -= LIVE_REDRAW * 2;
        let mut second = downloading("source", "example.org", 2);
        second.message = "second byte".to_string();
        sink.write_event(&second);
        let live = state.lock().unwrap();
        assert_eq!(live.fetch_progress.live_bytes(), 2);
        assert!(live.slots[0].bar.message().contains("second byte"));
    }

    #[test]
    fn remote_secondary_event_uses_a_source_activity_row() {
        let sink = ProgressSink::live_hidden(PathBuf::from("/run"));
        sink.write_event(&live_run_record(
            BuildStatus::RunStarted,
            json!({ "reachable": 1, "reachable_sources": 0, "jobs": 1 }),
        ));
        let event = BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::Running,
            op: Some("content".to_string()),
            message: "fetching object from potato".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: json!({
                "host": "potato",
                "transfer": "network",
                "content_source": "potato",
            })
            .as_object()
            .unwrap()
            .clone(),
        };
        let subject = SubjectIdentity::new("SecondaryContent", "object", "object-key");
        sink.write_event(&EventLogRecord::assemble(
            0,
            None,
            Some(&subject),
            &event,
            Path::new("/run"),
        ));

        let ProgressSink::Live(state) = &sink else {
            panic!("expected live sink");
        };
        let live = state.lock().unwrap();
        assert_eq!(live.fetch_progress.active(), 1);
        assert_eq!(
            live.slots.first().and_then(|slot| slot.activity.clone()),
            Some(ActivityKey::Source("object-key".to_string()))
        );
        assert_eq!(live.running(), 0);
    }

    #[test]
    fn the_hosts_line_names_the_bottleneck_and_then_retires() {
        // `ftp.gnu.org 1 (2 queued)` is the whole diagnosis: the host's slots
        // are full and the rest are behind it.
        let mut progress = FetchProgress::new(3);
        progress.handle(&queued("a", "ftp.gnu.org"));
        progress.handle(&queued("b", "ftp.gnu.org"));
        progress.handle(&queued("c", "ftp.gnu.org"));
        progress.handle(&downloading("a", "ftp.gnu.org", 10));
        let hosts = progress.render_hosts(FETCH_ASSUMED_WIDTH).unwrap();
        assert!(hosts.contains("ftp.gnu.org 1 (2 queued)"), "{hosts}");

        // Once nothing is queued the line goes; a retry re-queues a download
        // for a moment, and the line must not blink back for it.
        progress.handle(&downloading("b", "ftp.gnu.org", 10));
        progress.handle(&downloading("c", "ftp.gnu.org", 10));
        let rendered = progress.render();
        assert!(
            !rendered.iter().any(|line| line.starts_with("hosts")),
            "{rendered:?}"
        );
        progress.handle(&queued("d", "ftp.gnu.org"));
        let rendered = progress.render();
        assert!(
            !rendered.iter().any(|line| line.starts_with("hosts")),
            "{rendered:?}"
        );
    }

    #[test]
    fn a_host_with_nothing_waiting_shows_no_parenthesis() {
        // The parenthesis is the news; a host that is merely busy should not
        // wear one, so the eye goes straight to the backlog.
        let mut progress = FetchProgress::new(3);
        progress.handle(&queued("a", "quiet.example"));
        progress.handle(&downloading("a", "quiet.example", 1));
        progress.handle(&queued("b", "busy.example"));
        progress.handle(&queued("c", "busy.example"));
        progress.handle(&downloading("b", "busy.example", 1));

        let hosts = progress.render_hosts(FETCH_ASSUMED_WIDTH).unwrap();
        // Worst backlog first, whatever the alphabet says.
        assert!(
            hosts.starts_with("hosts  busy.example 1 (1 queued)"),
            "{hosts}"
        );
        assert!(hosts.contains("quiet.example 1"), "{hosts}");
        assert!(!hosts.contains("quiet.example 1 ("), "{hosts}");
    }

    #[test]
    fn slow_lines_hold_their_columns() {
        // Constant widths: the eye reads down the columns, so they must not
        // move when a name is long or a size grows a digit.
        let mut progress = FetchProgress::new(2);
        let long = "evolution-data-server-src-3.56.2-with-a-tail";
        progress.handle(&queued(long, "a.example"));
        progress.handle(&fetch_record(
            BuildStatus::Running,
            BuildLogLevel::Progress,
            Some((long, long)),
            json!({ "host": "a.example", "bytes": 1288490188u64, "total_bytes": 2040109465u64 }),
        ));
        progress.handle(&queued("short", "b.example"));
        progress.handle(&downloading("short", "b.example", 12 * 1024 * 1024));

        let lines = progress.render_slow(Instant::now(), FETCH_ASSUMED_WIDTH);
        assert_eq!(lines.len(), 2);
        // Truncated with a visible cut, and both lines break into columns at
        // the same offsets.
        assert!(
            lines[0].contains("evolution-data-server-src-3.."),
            "{lines:?}"
        );
        assert!(lines[0].contains("1.2/1.9 GB"), "{lines:?}");
        let host_column = |line: &str| line.rfind(".example").unwrap();
        assert_eq!(host_column(&lines[0]), host_column(&lines[1]), "{lines:?}");
    }

    #[test]
    fn the_slow_lines_are_the_oldest_downloads_not_the_newest() {
        // The old block showed whichever subjects happened to fit; this one
        // shows the ones that have been running longest, which is where a
        // stall shows up.
        let mut progress = FetchProgress::new(5);
        for name in ["first", "second", "third", "fourth"] {
            progress.handle(&queued(name, "example.org"));
            progress.handle(&downloading(name, "example.org", 1));
            std::thread::sleep(Duration::from_millis(2));
        }
        let lines = progress.render_slow(Instant::now(), FETCH_ASSUMED_WIDTH);
        assert_eq!(lines.len(), FETCH_SLOW_LINES);
        assert!(lines[0].contains("first"), "{lines:?}");
        assert!(lines[2].contains("third"), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("fourth")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_run_level_done_counts_towards_the_total() {
        // The fetcher reports a skipped Path source as a run-level `done`: it
        // belongs to no subject, but it is one of the sources the header
        // counts.
        let mut progress = FetchProgress::new(2);
        progress.handle(&fetch_record(
            BuildStatus::Done,
            BuildLogLevel::Info,
            None,
            json!({}),
        ));
        progress.handle(&fetch_record(
            BuildStatus::CacheHit,
            BuildLogLevel::Info,
            None,
            json!({}),
        ));
        assert_eq!(progress.done, 2);
    }

    #[test]
    fn quiet_silences_the_aggregate_heartbeat() {
        // `quiet` means "only what needs attention". The heartbeat is the
        // aggregate view's routine chatter off a terminal, so it belongs to the
        // half that goes silent -- otherwise the flag would look ignored, since
        // the heartbeat is the only thing such a run prints.
        let quiet = ProgressSink::Plain {
            run_log_dir: PathBuf::from("/run"),
            min_level: stderr_min_level(true),
            aggregate: Mutex::new(None),
        };
        let loud = ProgressSink::Plain {
            run_log_dir: PathBuf::from("/run"),
            min_level: stderr_min_level(false),
            aggregate: Mutex::new(None),
        };
        let start = fetch_record(
            BuildStatus::RunStarted,
            BuildLogLevel::Info,
            None,
            json!({ "progress": "aggregate", "sources": 2 }),
        );
        for sink in [&quiet, &loud] {
            sink.write_event(&start);
        }

        // Both are following the run; only their willingness to speak differs.
        let printed = |sink: &ProgressSink| match sink {
            ProgressSink::Plain { aggregate, .. } => {
                let mut guard = aggregate.lock().unwrap();
                let state = guard.as_mut().unwrap();
                // Force the heartbeat to be due.
                state.last_printed -= PLAIN_AGGREGATE_HEARTBEAT * 2;
                state.progress.total
            }
            _ => unreachable!(),
        };
        assert_eq!(printed(&quiet), 2);
        assert_eq!(printed(&loud), 2);

        let tick = fetch_record(
            BuildStatus::Running,
            BuildLogLevel::Progress,
            Some(("a", "a")),
            json!({ "host": "example.org", "bytes": 10 }),
        );
        quiet.write_event(&tick);
        loud.write_event(&tick);

        // The loud one spoke and reset its clock; the quiet one left it due.
        let still_due = |sink: &ProgressSink| match sink {
            ProgressSink::Plain { aggregate, .. } => {
                let guard = aggregate.lock().unwrap();
                let state = guard.as_ref().unwrap();
                Instant::now().duration_since(state.last_printed) >= PLAIN_AGGREGATE_HEARTBEAT
            }
            _ => unreachable!(),
        };
        assert!(still_due(&quiet), "quiet printed a heartbeat");
        assert!(!still_due(&loud), "the ordinary sink stayed silent");
    }

    #[test]
    fn a_narrow_terminal_trims_the_host_not_the_columns() {
        // The last column takes what is left, so a terminal too narrow for the
        // line loses the host -- never the identity of what is slow, and never
        // half of the "N more" tail.
        let mut progress = FetchProgress::new(2);
        progress.handle(&queued("some-source-1.2.3", "mirrors.kernel.org"));
        progress.handle(&downloading("some-source-1.2.3", "mirrors.kernel.org", 1));

        let narrow = progress.render_slow(Instant::now(), 72);
        assert!(narrow[0].chars().count() <= 72, "{narrow:?}");
        assert!(narrow[0].contains("some-source-1.2.3"), "{narrow:?}");
        assert!(narrow[0].trim_end().ends_with(".."), "{narrow:?}");

        let wide = progress.render_slow(Instant::now(), 120);
        assert!(wide[0].contains("mirrors.kernel.org"), "{wide:?}");
    }

    #[test]
    fn the_hosts_line_keeps_its_tail_whole() {
        // Five hosts will not fit a narrow line; what must survive is the
        // count of what was left out.
        let mut progress = FetchProgress::new(10);
        for (index, host) in [
            "mirrors.kernel.org",
            "static.crates.io",
            "gitlab.freedesktop.org",
            "download.gnome.org",
            "ftp.gnu.org",
        ]
        .iter()
        .enumerate()
        {
            let name = format!("source-{index}");
            progress.handle(&queued(&name, host));
            progress.handle(&queued(&format!("{name}-waiting"), host));
        }

        let narrow = progress.render_hosts(72).unwrap();
        assert!(narrow.chars().count() <= 72, "{narrow}");
        assert!(narrow.ends_with(" more"), "{narrow}");
    }

    #[test]
    fn byte_and_duration_formats_stay_readable() {
        // One unit for both halves: "1.2/1.9 GB" rather than "1.2 GB/1.9 GB",
        // which is longer and no clearer.
        assert_eq!(
            format_progress_size(1_288_490_188, Some(2_040_109_465)),
            "1.2/1.9 GB"
        );
        assert_eq!(format_progress_size(12 * 1024 * 1024, None), "12.0 MB");
        // The widest the column can get: a byte below the next unit still
        // rounds up to four digits on both halves.
        assert_eq!(
            format_progress_size(1_073_741_823, Some(1_073_741_823)),
            "1024.0/1024.0 MB"
        );
        assert!(format_progress_size(1_073_741_823, Some(1_073_741_823)).len() <= FETCH_SIZE_WIDTH);
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789abc", 10), "01234567..");
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1_938_859_476), "1.8 GB");
        assert_eq!(format_duration(Duration::from_secs(9)), "0:09");
        assert_eq!(format_duration(Duration::from_secs(511)), "8:31");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn a_missing_total_reads_as_zero_rather_than_vanishing() {
        // Details are best-effort within a build's run event: one absent
        // counter reads as zero. But the suffix as a whole appears only for
        // events that carry build counters at all -- another program's run
        // (the fetcher's) composes its own summary message, and stamping
        // "0 built" onto it would be an invention.
        let run_log_dir = PathBuf::from("/run");
        let assemble = |details: Map<String, Value>, message: &str| {
            let event = BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::RunFinished,
                op: None,
                message: message.to_string(),
                object_hash: None,
                raw_log_path: None,
                details,
            };
            EventLogRecord::assemble(0, None, None, &event, &run_log_dir)
        };

        let partial = assemble(
            Map::from_iter([("built".to_string(), Value::from(3_u64))]),
            "build finished",
        );
        assert!(
            format_progress_line(&partial, &run_log_dir)
                .ends_with("3 built · 0 cache-hit · 0 failed")
        );

        let foreign = assemble(Map::new(), "fetch finished: 5 downloaded");
        assert!(
            format_progress_line(&foreign, &run_log_dir).ends_with("fetch finished: 5 downloaded")
        );
    }

    #[test]
    fn progress_events_are_screen_only_and_keep_seq_contiguous() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs").join("260603123456");
        let logger = Arc::new(
            BuildRunLogger::new(&run_log_dir, "2026-06-03T12:34:56.000000000Z", true).unwrap(),
        );

        let info = |status| BuildLogEvent {
            level: BuildLogLevel::Info,
            status,
            op: None,
            message: "m".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        };

        logger.log_run_event(info(BuildStatus::RunStarted)); // durable seq 0
        logger.log_run_event(BuildLogEvent {
            level: BuildLogLevel::Progress,
            status: BuildStatus::Running,
            op: Some("download".to_string()),
            message: "12 MB".to_string(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        }); // transient: not persisted, no durable seq consumed
        logger.log_run_event(info(BuildStatus::RunFinished)); // durable seq 1

        logger.flush();
        let lines: Vec<Value> = run_event_log(&run_log_dir)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        // Progress is absent from the file; durable seq stays contiguous (0, 1).
        let seqs: Vec<u64> = lines.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![0, 1]);
        assert!(
            lines
                .iter()
                .all(|e| e["level"] != Value::String("progress".to_string()))
        );
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
