use crate::ObjectHash;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;
use time::macros::format_description;

/// Schema tag stamped on every on-disk event record.
pub const BUILD_EVENT_SCHEMA: &str = "bobr-build-event-v1";

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
    /// Creates a run logger writing under `run_log_dir`, tagged with `run_id`.
    /// `quiet` raises the stderr threshold (warnings/errors only). Sets up the
    /// file and progress sinks.
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
    },
}

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
    fn new(run_log_dir: PathBuf, quiet: bool) -> Self {
        if !quiet && std::io::stderr().is_terminal() {
            Self::Live(Box::new(Mutex::new(LiveProgress::new(
                run_log_dir,
                MultiProgress::new(),
            ))))
        } else {
            Self::Plain {
                run_log_dir,
                min_level: stderr_min_level(quiet),
            }
        }
    }

    /// Live sink with a hidden draw target, for tests: exercises the adapter
    /// without touching a terminal.
    #[cfg(test)]
    fn live_hidden(run_log_dir: PathBuf) -> Self {
        let multi = MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden());
        Self::Live(Box::new(Mutex::new(LiveProgress::new(run_log_dir, multi))))
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

/// One terminal line in the live block: a bar plus whether it currently holds a
/// running subject. A finished subject leaves its slot in place (marked idle)
/// for the next subject to reuse, so the block grows to the peak concurrency and
/// never shrinks mid-run.
struct Slot {
    bar: ProgressBar,
    occupied: bool,
}

/// Live indicatif state: a fixed set of subject slots (idle ones kept in place)
/// plus a bottom summary bar.
struct LiveProgress {
    run_log_dir: PathBuf,
    multi: MultiProgress,
    summary: ProgressBar,
    active_style: ProgressStyle,
    idle_style: ProgressStyle,
    slots: Vec<Slot>,
    index_of: HashMap<String, usize>,
    total: usize,
    done: usize,
    failed: usize,
}

impl LiveProgress {
    fn new(run_log_dir: PathBuf, multi: MultiProgress) -> Self {
        let summary = multi.add(ProgressBar::new_spinner());
        summary.set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
        Self {
            run_log_dir,
            multi,
            summary,
            active_style: ProgressStyle::with_template("{spinner} {msg} ({elapsed})")
                .expect("valid template"),
            idle_style: ProgressStyle::with_template("  {msg}").expect("valid template"),
            slots: Vec::new(),
            index_of: HashMap::new(),
            total: 0,
            done: 0,
            failed: 0,
        }
    }

    fn detail_u64(record: &EventLogRecord, key: &str) -> u64 {
        record.details.get(key).and_then(Value::as_u64).unwrap_or(0)
    }

    fn running(&self) -> usize {
        self.index_of.len()
    }

    fn update_summary(&self) {
        self.summary.set_message(format!(
            "{}/{} done · {} running · {} failed",
            self.done,
            self.total,
            self.running(),
            self.failed
        ));
    }

    /// Routes a subject to its existing slot, an idle slot, or a new bottom slot
    /// and sets the line text. Idle slots are reused in place, so the block does
    /// not grow past the peak number of concurrent subjects.
    fn start_or_update(&mut self, build_key: &str, message: String) {
        if let Some(&index) = self.index_of.get(build_key) {
            self.slots[index].bar.set_message(message);
            return;
        }
        let index = match self.slots.iter().position(|slot| !slot.occupied) {
            Some(index) => index,
            None => {
                let bar = self
                    .multi
                    .insert_before(&self.summary, ProgressBar::new_spinner());
                self.slots.push(Slot {
                    bar,
                    occupied: false,
                });
                self.slots.len() - 1
            }
        };
        let style = self.active_style.clone();
        let slot = &mut self.slots[index];
        slot.occupied = true;
        slot.bar.set_style(style);
        // Count `{elapsed}` from this subject's assignment, not from a previous
        // occupant of a reused slot.
        slot.bar.reset_elapsed();
        slot.bar.enable_steady_tick(Duration::from_millis(120));
        slot.bar.set_message(message);
        self.index_of.insert(build_key.to_string(), index);
        self.update_summary();
    }

    /// Marks a finished subject's slot idle: the line stays in place (the block
    /// does not shrink) and becomes available for the next subject.
    fn finish_subject(&mut self, build_key: &str, failed: bool) {
        if let Some(index) = self.index_of.remove(build_key) {
            let style = self.idle_style.clone();
            let slot = &mut self.slots[index];
            slot.occupied = false;
            slot.bar.disable_steady_tick();
            slot.bar.set_style(style);
            slot.bar.set_message("—");
        }
        if failed {
            self.failed += 1;
        } else {
            self.done += 1;
        }
        self.update_summary();
    }

    fn clear(&mut self) {
        for slot in self.slots.drain(..) {
            slot.bar.finish_and_clear();
        }
        self.index_of.clear();
        self.summary.finish_and_clear();
    }

    fn handle(&mut self, record: &EventLogRecord) {
        let status = record.status.as_str();

        if status == BuildStatus::RunStarted.as_str() {
            self.total = Self::detail_u64(record, "subjects") as usize;
            self.update_summary();
            return;
        }
        if status == BuildStatus::RunFinished.as_str() {
            self.clear();
            let _ = self.multi.println(format!(
                "done: {} built · {} cache-hit · {} failed",
                Self::detail_u64(record, "built"),
                Self::detail_u64(record, "cache_hit"),
                Self::detail_u64(record, "failed"),
            ));
            return;
        }
        if status == BuildStatus::CacheHit.as_str() {
            self.done += 1;
            self.update_summary();
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

        if status == BuildStatus::Done.as_str() {
            self.finish_subject(&subject.build_key, false);
            return;
        }
        if status == BuildStatus::Failed.as_str() {
            // Leave a visible record of the failure above the block.
            let _ = self
                .multi
                .println(format_progress_line(record, &self.run_log_dir));
            self.finish_subject(&subject.build_key, true);
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
        self.start_or_update(&subject.build_key, message);
    }
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
            } => {
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

    #[test]
    fn live_progress_keeps_idle_slots_and_reuses_them() {
        let sink = ProgressSink::live_hidden(PathBuf::from("/run"));
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
            assert_eq!(live.done, 1);
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
        assert_eq!(live.done, 3, "A + B done, plus one cache-hit");
        assert_eq!(live.failed, 1);
    }

    #[test]
    fn sink_uses_plain_path_off_tty() {
        // Skip under a real terminal (rare for `cargo test`, but be robust).
        if std::io::stderr().is_terminal() {
            return;
        }
        assert!(matches!(
            ProgressSink::new(PathBuf::from("/run"), false),
            ProgressSink::Plain {
                min_level: BuildLogLevel::Info,
                ..
            }
        ));
        assert!(matches!(
            ProgressSink::new(PathBuf::from("/run"), true),
            ProgressSink::Plain {
                min_level: BuildLogLevel::Warn,
                ..
            }
        ));
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
