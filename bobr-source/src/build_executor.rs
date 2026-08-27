//! Bounded in-process execution of synchronous builders.
//!
//! The asynchronous Realizer owns cache policy, dependency scheduling, and
//! publication. This module owns only the blocking builder call: jobs cross a
//! bounded channel, a dedicated dispatcher preserves FIFO order and limits
//! active worker threads, and each completion returns a guarded staged output.

use crate::graph::PlannedNode;
use bobr_builder::{BuilderInputs, BuilderPlannedSubject};
use bobr_core::{
    BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, BuildSeed, BuildStatus,
    CancellationToken, NoopBuildLogger, ObjectHash, ReuseKey, Run, RuntimeProvider,
    SubjectIdentity, SubjectRunContext,
};
use bobr_store::{Store, import_build};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

/// Stable identity of one submission to a [`BuildExecutor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuilderJobId(u64);

impl fmt::Display for BuilderJobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Failure before, during, or after synchronous builder execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildExecutorError {
    /// Executor configuration or job shape is invalid.
    InvalidRequest(String),
    /// The executor no longer accepts work.
    Closed(String),
    /// The job was cancelled before producing a usable staged output.
    Cancelled(String),
    /// Executor thread setup failed.
    Setup(String),
    /// Run workspace allocation or scratch management failed.
    Run(String),
    /// Subject logger setup failed.
    Logging(String),
    /// Builder implementation returned an error.
    Build(String),
    /// Builder worker panicked.
    Panic(String),
    /// Staged output could not be published into the working store.
    Publish(String),
}

impl fmt::Display for BuildExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message)
            | Self::Closed(message)
            | Self::Cancelled(message)
            | Self::Setup(message)
            | Self::Run(message)
            | Self::Logging(message)
            | Self::Build(message)
            | Self::Panic(message)
            | Self::Publish(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for BuildExecutorError {}

/// Everything needed for one synchronous builder call except the subject.
///
/// Cache resolution has already happened before this value is created. Inputs
/// are materialized filesystem paths, and `reuse_key` is used only to derive
/// the deterministic builder seed and later publish the successful result.
pub struct BuilderExecution {
    inputs: BuilderInputs,
    input_hashes: BTreeMap<String, ObjectHash>,
    reuse_key: ReuseKey,
    store: Store,
    run: Arc<Run>,
    run_logger: Arc<BuildRunLogger>,
    runtime_provider: RuntimeProvider,
    cancellation: CancellationToken,
}

impl fmt::Debug for BuilderExecution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BuilderExecution")
            .field("input_hashes", &self.input_hashes)
            .field("reuse_key", &self.reuse_key)
            .field("store", &self.store)
            .field("run", &self.run)
            .field("runtime_provider", &self.runtime_provider)
            .field("cancellation", &self.cancellation)
            .finish_non_exhaustive()
    }
}

impl BuilderExecution {
    /// Creates one cache-free builder execution request.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inputs: BuilderInputs,
        input_hashes: BTreeMap<String, ObjectHash>,
        reuse_key: ReuseKey,
        store: Store,
        run: Arc<Run>,
        run_logger: Arc<BuildRunLogger>,
        runtime_provider: RuntimeProvider,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            inputs,
            input_hashes,
            reuse_key,
            store,
            run,
            run_logger,
            runtime_provider,
            cancellation,
        }
    }
}

/// Owned executor job containing one planned builder and its execution data.
#[derive(Debug)]
pub struct BuilderJob {
    subject: Arc<PlannedNode>,
    execution: BuilderExecution,
}

impl BuilderJob {
    /// Creates a job and rejects a Source node at the executor boundary.
    pub fn new(
        subject: Arc<PlannedNode>,
        execution: BuilderExecution,
    ) -> Result<Self, BuildExecutorError> {
        if subject.as_builder().is_none() {
            return Err(BuildExecutorError::InvalidRequest(format!(
                "BuildExecutor cannot execute Source node '{}'",
                subject.name()
            )));
        }
        let builder = subject
            .as_builder()
            .expect("builder variant was checked above");
        let declared_inputs = builder
            .inputs()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let realized_inputs = execution
            .input_hashes
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if declared_inputs != realized_inputs {
            return Err(BuildExecutorError::InvalidRequest(format!(
                "builder '{}' input hashes do not match planned inputs: expected [{}], got [{}]",
                subject.name(),
                declared_inputs.join(", "),
                realized_inputs.join(", ")
            )));
        }
        Ok(Self { subject, execution })
    }

    fn cancellation(&self) -> CancellationToken {
        self.execution.cancellation.clone()
    }

    /// Announces acceptance into the executor FIFO before a worker can start.
    ///
    /// This is deliberately a transient run-level subject event: no workspace
    /// exists yet, so there cannot be a subject log file. The live renderer
    /// uses it to distinguish workers truly running from builders waiting for
    /// a bounded executor slot.
    fn log_queued(&self) {
        let builder = self
            .subject
            .as_builder()
            .expect("BuilderJob was validated before submission");
        let identity = SubjectIdentity::new(
            builder.tag(),
            builder.name(),
            builder.build_key().to_string(),
        );
        self.execution.run_logger.log_subject_event(
            &identity,
            BuildLogEvent {
                level: BuildLogLevel::Progress,
                status: BuildStatus::CacheMiss,
                op: Some("queued".to_string()),
                message: "waiting for builder slot".to_string(),
                object_hash: None,
                raw_log_path: None,
                details: serde_json::Map::from_iter([(
                    "queued_for_builder".to_string(),
                    serde_json::Value::Bool(true),
                )]),
            },
        );
    }
}

/// Successful synchronous builder result that has not yet entered the store.
///
/// The value owns the scratch-directory guard. Dropping it before or after
/// publication removes the workspace temp directory exactly once.
#[derive(Debug)]
pub struct StagedBuilderOutput {
    build_key: BuildKey,
    name: String,
    reuse_key: ReuseKey,
    input_hashes: BTreeMap<String, ObjectHash>,
    staged_path: PathBuf,
    run_id: String,
    started_at: Instant,
    terminal: BuilderTerminalGuard,
    _scratch: BuilderScratchGuard,
}

impl StagedBuilderOutput {
    /// Returns the builder output's build key.
    pub fn build_key(&self) -> BuildKey {
        self.build_key
    }

    /// Returns the path that is waiting to be imported.
    pub fn staged_path(&self) -> &Path {
        &self.staged_path
    }

    /// Returns the logger bound to the builder subject.
    pub fn logger(&self) -> &Arc<dyn BuildLogger> {
        self.terminal.logger()
    }
}

struct BuilderTerminalGuard {
    logger: Arc<dyn BuildLogger>,
    cancellation: CancellationToken,
    terminal: bool,
    unfinished_message: &'static str,
}

impl fmt::Debug for BuilderTerminalGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BuilderTerminalGuard")
            .field("cancellation", &self.cancellation)
            .field("terminal", &self.terminal)
            .field("unfinished_message", &self.unfinished_message)
            .finish_non_exhaustive()
    }
}

impl BuilderTerminalGuard {
    fn new(logger: Arc<dyn BuildLogger>, cancellation: CancellationToken) -> Self {
        Self {
            logger,
            cancellation,
            terminal: false,
            unfinished_message: "builder stopped without a terminal outcome",
        }
    }

    fn logger(&self) -> &Arc<dyn BuildLogger> {
        &self.logger
    }

    fn awaiting_publication(&mut self) {
        self.unfinished_message = "staged builder output was discarded before publication";
    }

    fn finish(
        &mut self,
        level: BuildLogLevel,
        status: BuildStatus,
        op: Option<&str>,
        message: impl Into<String>,
        object_hash: Option<ObjectHash>,
    ) {
        if self.terminal {
            return;
        }
        self.logger.log_event(BuildLogEvent {
            level,
            status,
            op: op.map(ToOwned::to_owned),
            message: message.into(),
            object_hash,
            raw_log_path: None,
            details: serde_json::Map::new(),
        });
        self.terminal = true;
    }
}

impl Drop for BuilderTerminalGuard {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        if self.cancellation.is_cancelled() {
            self.finish(
                BuildLogLevel::Info,
                BuildStatus::Cancelled,
                None,
                "subject cancelled; staged output discarded",
                None,
            );
        } else {
            self.finish(
                BuildLogLevel::Error,
                BuildStatus::Failed,
                None,
                self.unfinished_message,
                None,
            );
        }
    }
}

/// Imported builder result returned to the DAG scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedBuilderOutput {
    /// Build identity that was published.
    pub build_key: BuildKey,
    /// Content identity imported into the working store.
    pub object_hash: ObjectHash,
}

/// Publishes a staged builder output under its build and reuse identities.
///
/// This is deliberately separate from [`BuildExecutor`]. The Realizer calls
/// it from its local-I/O path and marks the DAG node ready only after success.
pub fn publish_builder_output(
    store: &Store,
    mut output: StagedBuilderOutput,
) -> Result<PublishedBuilderOutput, BuildExecutorError> {
    if let Err(error) = check_cancelled(&output.terminal.cancellation) {
        output.terminal.finish(
            BuildLogLevel::Info,
            BuildStatus::Cancelled,
            Some("publish"),
            error.to_string(),
            None,
        );
        return Err(error);
    }
    let result = import_build(
        store,
        output.build_key,
        output.reuse_key,
        output.input_hashes.values().copied().collect(),
        &output.staged_path,
        &output.name,
        &output.run_id,
    );
    match result {
        Ok(object_hash) => {
            output.terminal.finish(
                BuildLogLevel::Info,
                BuildStatus::Done,
                Some("publish"),
                format!(
                    "subject completed in {:.1}s",
                    output.started_at.elapsed().as_secs_f64()
                ),
                Some(object_hash),
            );
            Ok(PublishedBuilderOutput {
                build_key: output.build_key,
                object_hash,
            })
        }
        Err(error) => {
            output.terminal.finish(
                BuildLogLevel::Error,
                BuildStatus::Failed,
                Some("publish"),
                format!("failed to publish builder output: {error}"),
                None,
            );
            Err(BuildExecutorError::Publish(error.to_string()))
        }
    }
}

/// Executes one builder synchronously without cache lookup or publication.
///
/// The function is shared by the transitional executor and worker threads so
/// there is only one implementation of workspace, logging, and scratch
/// lifetime management.
pub fn execute_builder_staged(
    subject: &BuilderPlannedSubject,
    execution: BuilderExecution,
) -> Result<StagedBuilderOutput, BuildExecutorError> {
    let started_at = Instant::now();
    check_cancelled(&execution.cancellation)?;
    let workspace = execution
        .run
        .create_workspace(
            subject.tag(),
            subject.name(),
            subject.build_key().to_string(),
        )
        .map_err(|error| BuildExecutorError::Run(error.to_string()))?;
    let mut scratch =
        BuilderScratchGuard::new(execution.run.clone(), workspace.temp_dir().to_path_buf());
    let logger = execution
        .run_logger
        .bind_subject(subject.log_subject(&workspace))
        .map_err(BuildExecutorError::Logging)?;
    scratch.set_logger(logger.clone());
    let mut terminal = BuilderTerminalGuard::new(logger.clone(), execution.cancellation.clone());
    log_builder_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Start,
        None,
        "starting subject",
    );
    log_builder_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::CacheMiss,
        None,
        "executing builder",
    );
    if let Err(error) = execution.run.prepare_scratch(workspace.temp_dir()) {
        let error = BuildExecutorError::Run(error.to_string());
        terminal.finish(
            BuildLogLevel::Error,
            BuildStatus::Failed,
            None,
            error.to_string(),
            None,
        );
        return Err(error);
    }
    if let Err(error) = check_cancelled(&execution.cancellation) {
        terminal.finish(
            BuildLogLevel::Info,
            BuildStatus::Cancelled,
            None,
            error.to_string(),
            None,
        );
        return Err(error);
    }
    log_builder_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Running,
        None,
        "running builder implementation",
    );
    let context = SubjectRunContext::new(
        workspace,
        logger.clone(),
        execution.cancellation.clone(),
        execution.runtime_provider,
        BuildSeed::from_reuse_key(&execution.reuse_key),
    );
    let staged_path = match subject.execute(&context, execution.inputs, execution.store.fs_tree()) {
        Ok(path) => path,
        Err(error) => {
            let mapped = match error {
                bobr_builder::BuilderError::Cancelled(message) => {
                    BuildExecutorError::Cancelled(message)
                }
                other => BuildExecutorError::Build(other.to_string()),
            };
            let (level, status) = if matches!(mapped, BuildExecutorError::Cancelled(_)) {
                (BuildLogLevel::Info, BuildStatus::Cancelled)
            } else {
                (BuildLogLevel::Error, BuildStatus::Failed)
            };
            terminal.finish(level, status, None, mapped.to_string(), None);
            return Err(mapped);
        }
    };
    if let Err(error) = check_cancelled(&execution.cancellation) {
        terminal.finish(
            BuildLogLevel::Info,
            BuildStatus::Cancelled,
            None,
            error.to_string(),
            None,
        );
        return Err(error);
    }
    terminal.awaiting_publication();
    Ok(StagedBuilderOutput {
        build_key: subject.build_key(),
        name: subject.name().to_string(),
        reuse_key: execution.reuse_key,
        input_hashes: execution.input_hashes,
        staged_path,
        run_id: execution.run.run_id().to_string(),
        started_at,
        terminal,
        _scratch: scratch,
    })
}

/// Owning executor with a dedicated dispatcher thread.
#[derive(Debug)]
pub struct BuildExecutor {
    handle: BuildExecutorHandle,
    dispatcher: Option<JoinHandle<()>>,
}

impl BuildExecutor {
    /// Starts an executor with at most `jobs` active workers and a bounded
    /// submission queue of `queue_capacity` commands.
    pub fn new(jobs: usize, queue_capacity: usize) -> Result<Self, BuildExecutorError> {
        if jobs == 0 {
            return Err(BuildExecutorError::InvalidRequest(
                "BuildExecutor jobs must be greater than zero".to_string(),
            ));
        }
        if queue_capacity == 0 {
            return Err(BuildExecutorError::InvalidRequest(
                "BuildExecutor queue capacity must be greater than zero".to_string(),
            ));
        }
        let capacity = jobs.checked_add(queue_capacity).ok_or_else(|| {
            BuildExecutorError::InvalidRequest(
                "BuildExecutor jobs plus queue capacity overflowed usize".to_string(),
            )
        })?;
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let event_sender = sender.downgrade();
        let dispatcher = thread::Builder::new()
            .name("bobr-build-dispatcher".to_string())
            .spawn(move || dispatch(receiver, event_sender, jobs))
            .map_err(|error| {
                BuildExecutorError::Setup(format!("failed to start BuildExecutor: {error}"))
            })?;
        Ok(Self {
            handle: BuildExecutorHandle {
                sender,
                next_id: Arc::new(AtomicU64::new(0)),
                capacity: Arc::new(Semaphore::new(capacity)),
            },
            dispatcher: Some(dispatcher),
        })
    }

    /// Returns a cloneable asynchronous submission handle.
    pub fn handle(&self) -> BuildExecutorHandle {
        self.handle.clone()
    }

    /// Cancels queued and active work and waits for every active worker.
    pub async fn shutdown(mut self) -> Result<(), BuildExecutorError> {
        let (acknowledge, acknowledged) = oneshot::channel();
        self.handle
            .sender
            .send(Command::Shutdown { acknowledge })
            .await
            .map_err(|_| BuildExecutorError::Closed("BuildExecutor is closed".to_string()))?;
        acknowledged
            .await
            .map_err(|_| BuildExecutorError::Closed("BuildExecutor stopped unexpectedly".into()))?;
        let Some(dispatcher) = self.dispatcher.take() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || dispatcher.join())
            .await
            .map_err(|error| {
                BuildExecutorError::Panic(format!("dispatcher join task panicked: {error}"))
            })?
            .map_err(|_| BuildExecutorError::Panic("BuildExecutor dispatcher panicked".into()))?;
        Ok(())
    }
}

impl Drop for BuildExecutor {
    fn drop(&mut self) {
        let (acknowledge, _acknowledged) = oneshot::channel();
        let _ = self
            .handle
            .sender
            .try_send(Command::Shutdown { acknowledge });
    }
}

/// Cloneable asynchronous interface to the synchronous executor.
#[derive(Debug, Clone)]
pub struct BuildExecutorHandle {
    sender: mpsc::Sender<Command>,
    next_id: Arc<AtomicU64>,
    capacity: Arc<Semaphore>,
}

impl BuildExecutorHandle {
    /// Submits one builder in FIFO command order and returns its completion.
    pub async fn submit(&self, job: BuilderJob) -> Result<BuilderCompletion, BuildExecutorError> {
        // The first `jobs` permits account for active workers; the remaining
        // permits bound accepted FIFO entries even when the dispatcher drains
        // its command channel eagerly.
        let capacity = self.capacity.clone().acquire_owned().await.map_err(|_| {
            BuildExecutorError::Closed("BuildExecutor capacity is closed".to_string())
        })?;
        let id = BuilderJobId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (result, receiver) = oneshot::channel();
        self.sender
            .send(Command::Submit(PendingJob {
                id,
                job,
                result: Arc::new(Mutex::new(Some(result))),
                _capacity: capacity,
            }))
            .await
            .map_err(|_| BuildExecutorError::Closed("BuildExecutor is closed".to_string()))?;
        Ok(BuilderCompletion { id, receiver })
    }

    /// Requests cancellation of one queued or active job.
    pub async fn cancel(&self, id: BuilderJobId) -> Result<(), BuildExecutorError> {
        self.sender
            .send(Command::Cancel(id))
            .await
            .map_err(|_| BuildExecutorError::Closed("BuildExecutor is closed".to_string()))
    }
}

/// One submitted job and the future-like receiver for its staged result.
#[derive(Debug)]
pub struct BuilderCompletion {
    id: BuilderJobId,
    receiver: oneshot::Receiver<Result<StagedBuilderOutput, BuildExecutorError>>,
}

impl BuilderCompletion {
    /// Returns the assigned executor job ID.
    pub fn id(&self) -> BuilderJobId {
        self.id
    }

    /// Waits asynchronously without occupying a builder worker or Tokio thread.
    pub async fn wait(self) -> Result<StagedBuilderOutput, BuildExecutorError> {
        self.receiver.await.map_err(|_| {
            BuildExecutorError::Closed(format!(
                "BuildExecutor lost completion for job '{}'",
                self.id
            ))
        })?
    }
}

#[derive(Debug)]
enum Command {
    Submit(PendingJob),
    Cancel(BuilderJobId),
    Finished(BuilderJobId),
    Shutdown { acknowledge: oneshot::Sender<()> },
}

#[derive(Debug)]
struct PendingJob {
    id: BuilderJobId,
    job: BuilderJob,
    result: PendingResult,
    _capacity: OwnedSemaphorePermit,
}

type PendingResult =
    Arc<Mutex<Option<oneshot::Sender<Result<StagedBuilderOutput, BuildExecutorError>>>>>;

fn dispatch(
    mut receiver: mpsc::Receiver<Command>,
    event_sender: mpsc::WeakSender<Command>,
    jobs: usize,
) {
    let mut queue = VecDeque::<PendingJob>::new();
    let mut active = HashMap::<BuilderJobId, ActiveJob>::new();
    let mut shutting_down = false;
    let mut shutdown_acknowledge = None;

    while let Some(command) = receiver.blocking_recv() {
        match command {
            Command::Submit(pending) if !shutting_down => {
                pending.job.log_queued();
                queue.push_back(pending);
            }
            Command::Submit(pending) => {
                pending.job.cancellation().cancel();
                send_pending_result(
                    &pending.result,
                    Err(BuildExecutorError::Closed(
                        "BuildExecutor is shutting down".to_string(),
                    )),
                );
            }
            Command::Cancel(id) => {
                if let Some(index) = queue.iter().position(|pending| pending.id == id) {
                    let pending = queue.remove(index).expect("queued job index disappeared");
                    pending.job.cancellation().cancel();
                    send_pending_result(&pending.result, Err(cancelled_job(id)));
                } else if let Some(active_job) = active.get(&id) {
                    active_job.cancellation.cancel();
                }
            }
            Command::Finished(id) => {
                if let Some(active_job) = active.remove(&id) {
                    let _ = active_job.worker.join();
                }
            }
            Command::Shutdown { acknowledge } => {
                shutting_down = true;
                shutdown_acknowledge.get_or_insert(acknowledge);
                while let Some(pending) = queue.pop_front() {
                    pending.job.cancellation().cancel();
                    send_pending_result(
                        &pending.result,
                        Err(BuildExecutorError::Closed(
                            "BuildExecutor shut down before starting the job".to_string(),
                        )),
                    );
                }
                for active_job in active.values() {
                    active_job.cancellation.cancel();
                }
            }
        }

        while !shutting_down && active.len() < jobs {
            let Some(pending) = queue.pop_front() else {
                break;
            };
            if pending.job.cancellation().is_cancelled() {
                send_pending_result(&pending.result, Err(cancelled_job(pending.id)));
                continue;
            }
            let PendingJob {
                id,
                job,
                result,
                _capacity: capacity,
            } = pending;
            let cancellation = job.cancellation();
            let Some(sender) = event_sender.upgrade() else {
                send_pending_result(
                    &result,
                    Err(BuildExecutorError::Closed(
                        "BuildExecutor event channel is closed".to_string(),
                    )),
                );
                continue;
            };
            let result_sender = result.clone();
            let spawn_error_sender = result;
            let spawn_result = thread::Builder::new()
                .name(format!("bobr-builder-{id}"))
                .spawn(move || {
                    let result = panic::catch_unwind(AssertUnwindSafe(|| {
                        let subject = job
                            .subject
                            .as_builder()
                            .expect("BuilderJob was validated before submission");
                        execute_builder_staged(subject, job.execution)
                    }))
                    .unwrap_or_else(|_| {
                        Err(BuildExecutorError::Panic(format!(
                            "builder worker for job '{id}' panicked"
                        )))
                    });
                    send_pending_result(&result_sender, result);
                    let _ = sender.blocking_send(Command::Finished(id));
                    drop(capacity);
                });
            match spawn_result {
                Ok(worker) => {
                    active.insert(
                        id,
                        ActiveJob {
                            cancellation,
                            worker,
                        },
                    );
                }
                Err(error) => {
                    send_pending_result(
                        &spawn_error_sender,
                        Err(BuildExecutorError::Setup(format!(
                            "failed to start builder worker for job '{id}': {error}"
                        ))),
                    );
                }
            }
        }

        if shutting_down && active.is_empty() {
            if let Some(acknowledge) = shutdown_acknowledge.take() {
                let _ = acknowledge.send(());
            }
            break;
        }
    }
}

#[derive(Debug)]
struct ActiveJob {
    cancellation: CancellationToken,
    worker: JoinHandle<()>,
}

fn send_pending_result(
    sender: &PendingResult,
    result: Result<StagedBuilderOutput, BuildExecutorError>,
) {
    let sender = sender
        .lock()
        .expect("builder result sender poisoned")
        .take();
    if let Some(sender) = sender {
        let _ = sender.send(result);
    }
}

fn cancelled_job(id: BuilderJobId) -> BuildExecutorError {
    BuildExecutorError::Cancelled(format!("builder job '{id}' was cancelled"))
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), BuildExecutorError> {
    if cancellation.is_cancelled() {
        Err(BuildExecutorError::Cancelled(
            "build cancelled by signal".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn log_builder_event(
    logger: &dyn BuildLogger,
    level: BuildLogLevel,
    status: BuildStatus,
    op: Option<&str>,
    message: impl Into<String>,
) {
    logger.log_event(BuildLogEvent {
        level,
        status,
        op: op.map(ToOwned::to_owned),
        message: message.into(),
        object_hash: None,
        raw_log_path: None,
        details: serde_json::Map::new(),
    });
}

#[derive(Debug)]
struct BuilderScratchGuard {
    run: Arc<Run>,
    scratch_dir: PathBuf,
    logger: Arc<dyn BuildLogger>,
}

impl BuilderScratchGuard {
    fn new(run: Arc<Run>, scratch_dir: PathBuf) -> Self {
        Self {
            run,
            scratch_dir,
            logger: Arc::new(NoopBuildLogger),
        }
    }

    fn set_logger(&mut self, logger: Arc<dyn BuildLogger>) {
        self.logger = logger;
    }
}

impl Drop for BuilderScratchGuard {
    fn drop(&mut self) {
        if let Err(error) = self.run.remove_scratch(&self.scratch_dir) {
            log_builder_event(
                self.logger.as_ref(),
                BuildLogLevel::Warn,
                BuildStatus::Cleanup,
                Some("cleanup"),
                format!(
                    "failed to remove temp dir '{}': {error}",
                    self.scratch_dir.display()
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_builder::{BuildContext, BuilderError, InputSpec, TypedBuilder};
    use bobr_store::load_build_object_hash;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::fs;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;
    use tempfile::{TempDir, tempdir};

    #[derive(Debug)]
    struct ProbeBuilder;

    #[derive(Debug, Clone, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct ProbeConfig {
        group: String,
        label: String,
        panic: bool,
    }

    static PROBE_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };
    static PROBE_BUILDER: ProbeBuilder = ProbeBuilder;
    static PROBES: LazyLock<Mutex<HashMap<String, Arc<ProbeState>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    #[derive(Debug)]
    struct ProbeState {
        active: AtomicUsize,
        maximum: AtomicUsize,
        release: AtomicBool,
        order: Mutex<Vec<String>>,
    }

    impl ProbeState {
        fn new(release: bool) -> Arc<Self> {
            Arc::new(Self {
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
                release: AtomicBool::new(release),
                order: Mutex::new(Vec::new()),
            })
        }
    }

    struct ActiveProbe(Arc<ProbeState>);

    impl Drop for ActiveProbe {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl TypedBuilder for ProbeBuilder {
        type Config = ProbeConfig;

        fn tag(&self) -> &'static str {
            "ExecutorProbe"
        }

        fn spec(&self) -> &'static InputSpec {
            &PROBE_SPEC
        }

        fn impl_version(&self) -> &'static str {
            "1"
        }

        fn build_typed(
            &self,
            config: Self::Config,
            _inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<PathBuf, BuilderError> {
            let probe = PROBES
                .lock()
                .expect("probe registry poisoned")
                .get(&config.group)
                .cloned()
                .expect("probe was not registered");
            probe
                .order
                .lock()
                .expect("probe order poisoned")
                .push(config.label.clone());
            let active = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
            probe.maximum.fetch_max(active, Ordering::SeqCst);
            let _active = ActiveProbe(probe.clone());
            if config.panic {
                panic!("requested probe panic");
            }
            while !probe.release.load(Ordering::SeqCst) {
                cx.check_cancelled()?;
                thread::sleep(Duration::from_millis(2));
            }
            cx.check_cancelled()?;
            let output = cx.temp_dir.join("output");
            fs::write(&output, config.label.as_bytes())
                .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
            Ok(output)
        }
    }

    struct TestEnvironment {
        _temp: TempDir,
        store: Store,
        run: Arc<Run>,
        logger: Arc<BuildRunLogger>,
    }

    fn environment(label: &str) -> TestEnvironment {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        let logs = temp.path().join("logs");
        let work = temp.path().join("work");
        fs::create_dir(&store_root).unwrap();
        fs::create_dir(&logs).unwrap();
        fs::create_dir(&work).unwrap();
        let store = Store::create(&store_root).unwrap();
        let run = Arc::new(Run::new(format!("executor-{label}"), &logs, &work).unwrap());
        let logger = Arc::new(BuildRunLogger::new(&logs, run.run_id(), true).unwrap());
        TestEnvironment {
            _temp: temp,
            store,
            run,
            logger,
        }
    }

    fn register_probe(group: &str, release: bool) -> Arc<ProbeState> {
        let probe = ProbeState::new(release);
        PROBES
            .lock()
            .expect("probe registry poisoned")
            .insert(group.to_string(), probe.clone());
        probe
    }

    fn job(
        environment: &TestEnvironment,
        group: &str,
        label: &str,
        should_panic: bool,
    ) -> (BuildKey, BuilderJob) {
        job_with_cancellation(
            environment,
            group,
            label,
            should_panic,
            CancellationToken::new(),
        )
    }

    fn job_with_cancellation(
        environment: &TestEnvironment,
        group: &str,
        label: &str,
        should_panic: bool,
        cancellation: CancellationToken,
    ) -> (BuildKey, BuilderJob) {
        let subject = BuilderPlannedSubject::new(
            &PROBE_BUILDER,
            format!("probe-{label}"),
            json!({"group": group, "label": label, "panic": should_panic}),
            BTreeMap::new(),
        )
        .unwrap();
        let build_key = subject.build_key();
        let reuse_key = subject.compute_reuse_key(&BTreeMap::new()).unwrap();
        let execution = BuilderExecution::new(
            BuilderInputs::empty(),
            BTreeMap::new(),
            reuse_key,
            environment.store.clone(),
            environment.run.clone(),
            environment.logger.clone(),
            RuntimeProvider::host(),
            cancellation,
        );
        (
            build_key,
            BuilderJob::new(Arc::new(PlannedNode::Builder(subject)), execution).unwrap(),
        )
    }

    fn subject_statuses(environment: &TestEnvironment, label: &str) -> Vec<String> {
        environment.logger.flush();
        fs::read_to_string(environment.run.logs_dir().join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["subject"]["name"] == format!("probe-{label}"))
            .map(|event| event["status"].as_str().unwrap().to_string())
            .collect()
    }

    async fn wait_for(predicate: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("condition timed out");
    }

    #[test]
    fn rejects_zero_limits_and_source_jobs() {
        assert!(matches!(
            BuildExecutor::new(0, 1),
            Err(BuildExecutorError::InvalidRequest(_))
        ));
        assert!(matches!(
            BuildExecutor::new(1, 0),
            Err(BuildExecutorError::InvalidRequest(_))
        ));
        let environment = environment("source-job");
        let source = crate::SourcePlannedSubject::new(
            "source".to_string(),
            "1".repeat(64).parse().unwrap(),
            None,
        );
        let execution = BuilderExecution::new(
            BuilderInputs::empty(),
            BTreeMap::new(),
            "2".repeat(64).parse().unwrap(),
            environment.store,
            environment.run,
            environment.logger,
            RuntimeProvider::host(),
            CancellationToken::new(),
        );
        assert!(matches!(
            BuilderJob::new(Arc::new(PlannedNode::Source(source)), execution),
            Err(BuildExecutorError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn dispatcher_preserves_fifo_order_for_queued_jobs() {
        let environment = environment("fifo");
        let probe = register_probe("fifo", false);
        let executor = BuildExecutor::new(1, 8).unwrap();
        let handle = executor.handle();
        let first = handle
            .submit(job(&environment, "fifo", "first", false).1)
            .await
            .unwrap();
        wait_for(|| probe.active.load(Ordering::SeqCst) == 1).await;
        let second = handle
            .submit(job(&environment, "fifo", "second", false).1)
            .await
            .unwrap();
        let third = handle
            .submit(job(&environment, "fifo", "third", false).1)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            *probe.order.lock().expect("probe order poisoned"),
            ["first"]
        );

        probe.release.store(true, Ordering::SeqCst);
        drop(first.wait().await.unwrap());
        drop(second.wait().await.unwrap());
        drop(third.wait().await.unwrap());
        assert_eq!(
            *probe.order.lock().expect("probe order poisoned"),
            ["first", "second", "third"]
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn active_workers_never_exceed_jobs() {
        let environment = environment("limit");
        let probe = register_probe("limit", false);
        let executor = BuildExecutor::new(2, 8).unwrap();
        let handle = executor.handle();
        let mut completions = Vec::new();
        for label in ["one", "two", "three", "four"] {
            completions.push(
                handle
                    .submit(job(&environment, "limit", label, false).1)
                    .await
                    .unwrap(),
            );
        }
        wait_for(|| probe.active.load(Ordering::SeqCst) == 2).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(probe.maximum.load(Ordering::SeqCst), 2);
        assert_eq!(probe.order.lock().expect("probe order poisoned").len(), 2);

        probe.release.store(true, Ordering::SeqCst);
        for completion in completions {
            drop(completion.wait().await.unwrap());
        }
        assert_eq!(probe.order.lock().expect("probe order poisoned").len(), 4);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_queue_applies_backpressure_to_submitters() {
        let environment = environment("backpressure");
        let probe = register_probe("backpressure", false);
        let executor = BuildExecutor::new(1, 1).unwrap();
        let handle = executor.handle();
        let active = handle
            .submit(job(&environment, "backpressure", "active", false).1)
            .await
            .unwrap();
        wait_for(|| probe.active.load(Ordering::SeqCst) == 1).await;
        let queued = handle
            .submit(job(&environment, "backpressure", "queued", false).1)
            .await
            .unwrap();
        assert_eq!(handle.capacity.available_permits(), 0);
        let third_job = job(&environment, "backpressure", "third", false).1;
        let third_handle = handle.clone();
        let third_submit = tokio::spawn(async move { third_handle.submit(third_job).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!third_submit.is_finished());
        probe.release.store(true, Ordering::SeqCst);
        drop(active.wait().await.unwrap());
        let third = third_submit.await.unwrap().unwrap();
        drop(queued.wait().await.unwrap());
        drop(third.wait().await.unwrap());
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_and_active_jobs_are_cancelled_without_blocking_tokio() {
        let environment = environment("cancel");
        let probe = register_probe("cancel", false);
        let executor = BuildExecutor::new(1, 8).unwrap();
        let handle = executor.handle();
        let active = handle
            .submit(job(&environment, "cancel", "active", false).1)
            .await
            .unwrap();
        wait_for(|| probe.active.load(Ordering::SeqCst) == 1).await;
        let queued = handle
            .submit(job(&environment, "cancel", "queued", false).1)
            .await
            .unwrap();

        handle.cancel(queued.id()).await.unwrap();
        assert!(matches!(
            queued.wait().await,
            Err(BuildExecutorError::Cancelled(_))
        ));
        handle.cancel(active.id()).await.unwrap();
        assert!(matches!(
            active.wait().await,
            Err(BuildExecutorError::Cancelled(_))
        ));
        tokio::time::timeout(
            Duration::from_millis(50),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap();
        executor.shutdown().await.unwrap();
        assert_eq!(
            *probe.order.lock().expect("probe order poisoned"),
            ["active"]
        );
        assert_eq!(
            subject_statuses(&environment, "active")
                .last()
                .map(String::as_str),
            Some("cancelled")
        );
        assert!(
            fs::read_dir(environment.run.work_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn dropped_waiter_still_gets_one_terminal_cancelled_event() {
        let environment = environment("dropped-waiter");
        let probe = register_probe("dropped-waiter", false);
        let executor = BuildExecutor::new(1, 2).unwrap();
        let handle = executor.handle();
        let (build_key, builder_job) = job(&environment, "dropped-waiter", "dropped-waiter", false);
        let completion = handle.submit(builder_job).await.unwrap();
        wait_for(|| probe.active.load(Ordering::SeqCst) == 1).await;

        handle.cancel(completion.id()).await.unwrap();
        drop(completion);
        executor.shutdown().await.unwrap();

        let statuses = subject_statuses(&environment, "dropped-waiter");
        assert_eq!(
            statuses
                .iter()
                .filter(|status| status.as_str() == "cancelled")
                .count(),
            1
        );
        assert!(!statuses.iter().any(|status| status == "done"));
        assert!(!statuses.iter().any(|status| status == "failed"));
        assert_eq!(
            load_build_object_hash(&environment.store, build_key).unwrap(),
            None
        );
        assert!(
            fs::read_dir(environment.run.work_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancellation_between_staging_and_publication_discards_the_mapping() {
        let environment = environment("cancel-staged");
        register_probe("cancel-staged", true);
        let cancellation = CancellationToken::new();
        let executor = BuildExecutor::new(1, 2).unwrap();
        let (build_key, builder_job) = job_with_cancellation(
            &environment,
            "cancel-staged",
            "cancel-staged",
            false,
            cancellation.clone(),
        );
        let staged = executor
            .handle()
            .submit(builder_job)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();

        cancellation.cancel();
        assert!(matches!(
            publish_builder_output(&environment.store, staged),
            Err(BuildExecutorError::Cancelled(_))
        ));
        executor.shutdown().await.unwrap();

        assert_eq!(
            subject_statuses(&environment, "cancel-staged")
                .last()
                .map(String::as_str),
            Some("cancelled")
        );
        assert_eq!(
            load_build_object_hash(&environment.store, build_key).unwrap(),
            None
        );
        assert!(
            fs::read_dir(environment.run.work_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_and_joins_active_workers() {
        let environment = environment("shutdown");
        let probe = register_probe("shutdown", false);
        let executor = BuildExecutor::new(1, 2).unwrap();
        let completion = executor
            .handle()
            .submit(job(&environment, "shutdown", "active", false).1)
            .await
            .unwrap();
        wait_for(|| probe.active.load(Ordering::SeqCst) == 1).await;

        executor.shutdown().await.unwrap();

        assert_eq!(probe.active.load(Ordering::SeqCst), 0);
        assert!(matches!(
            completion.wait().await,
            Err(BuildExecutorError::Cancelled(_))
        ));
    }

    #[tokio::test]
    async fn worker_panic_is_isolated_and_next_job_runs() {
        let environment = environment("panic");
        register_probe("panic", true);
        let executor = BuildExecutor::new(1, 4).unwrap();
        let handle = executor.handle();
        let broken = handle
            .submit(job(&environment, "panic", "broken", true).1)
            .await
            .unwrap();
        let healthy = handle
            .submit(job(&environment, "panic", "healthy", false).1)
            .await
            .unwrap();

        assert!(matches!(
            broken.wait().await,
            Err(BuildExecutorError::Panic(_))
        ));
        drop(healthy.wait().await.unwrap());
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn publication_happens_after_completion_and_cleans_scratch() {
        let environment = environment("publish");
        register_probe("publish", true);
        let executor = BuildExecutor::new(1, 4).unwrap();
        let (build_key, builder_job) = job(&environment, "publish", "result", false);
        let staged = executor
            .handle()
            .submit(builder_job)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let staged_path = staged.staged_path().to_path_buf();

        assert!(staged_path.is_file());
        assert_eq!(
            load_build_object_hash(&environment.store, build_key).unwrap(),
            None
        );
        let published = publish_builder_output(&environment.store, staged).unwrap();
        assert_eq!(published.build_key, build_key);
        assert_eq!(
            load_build_object_hash(&environment.store, build_key).unwrap(),
            Some(published.object_hash)
        );
        assert!(!staged_path.exists());
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn realizer_completion_path_publishes_outside_the_worker() {
        let environment = environment("realizer-publish");
        register_probe("realizer-publish", true);
        let executor = BuildExecutor::new(1, 4).unwrap();
        let (build_key, builder_job) =
            job(&environment, "realizer-publish", "realized-result", false);

        let published = crate::realizer::execute_builder_miss(
            &executor.handle(),
            builder_job,
            environment.store.clone(),
        )
        .await
        .unwrap();

        assert_eq!(published.build_key, build_key);
        assert_eq!(
            load_build_object_hash(&environment.store, build_key).unwrap(),
            Some(published.object_hash)
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_publication_does_not_publish_mapping_and_cleans_scratch() {
        let environment = environment("publish-failure");
        register_probe("publish-failure", true);
        let executor = BuildExecutor::new(1, 4).unwrap();
        let (build_key, builder_job) =
            job(&environment, "publish-failure", "missing-output", false);
        let staged = executor
            .handle()
            .submit(builder_job)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let staged_path = staged.staged_path().to_path_buf();
        fs::remove_file(&staged_path).unwrap();

        assert!(matches!(
            publish_builder_output(&environment.store, staged),
            Err(BuildExecutorError::Publish(_))
        ));
        assert_eq!(
            load_build_object_hash(&environment.store, build_key).unwrap(),
            None
        );
        assert!(!staged_path.exists());
        executor.shutdown().await.unwrap();
    }
}
