use crate::builder_registry::create_builder_registry;
use crate::collect_graph::collect_graph;
use crate::planned::{
    PlannedExecutionContext, PlannedSubject, RealizedInput, SubjectExecution, SubjectOutcome,
    execute_subject, realized_object_from_record,
};
use crate::request::Request;
use bobr_core::{
    BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, BuildStatus,
    CancellationToken, NoopBuildLogger, ObjectHash, RuntimeBackend, RuntimeProvider,
    SubjectIdentity,
};
use bobr_runtime::runtime_provider::runtime_provider_for_current_process;
use bobr_store::{RealizedObject, Store, StoreError, StoreTempDir, load_build_handle};
use mbuild_builder::{BuilderError, BuilderPlannedSubject};
use serde_json::to_string_pretty;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;

type SubjectGraph = HashMap<BuildKey, Arc<PlannedSubject>>;

#[derive(Debug)]
pub enum ExecutionError {
    InvalidRequest(String),
    UnknownBuilder(String),
    RequestLoad(String),
    Cancelled(String),
    Build(String),
    Store(String),
}

impl ExecutionError {
    fn class(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid-request",
            Self::UnknownBuilder(_) => "unknown-builder",
            Self::RequestLoad(_) => "request-load",
            Self::Cancelled(_) => "cancelled",
            Self::Build(_) => "build",
            Self::Store(_) => "store",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::UnknownBuilder(message)
            | Self::RequestLoad(message)
            | Self::Cancelled(message)
            | Self::Build(message)
            | Self::Store(message) => message,
        }
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ExecutionError {}

/// Prepares an empty temp directory for a builder run. Builders own their
/// staging area, so this runs before
/// `BuilderPlannedSubject::execute` constructs its `BuildContext`.
pub(crate) fn prepare_temp(temp_dir: &StoreTempDir) -> Result<(), ExecutionError> {
    temp_dir.prepare_empty().map_err(map_store_error)
}

pub(crate) fn map_builder_error(error: BuilderError) -> ExecutionError {
    match error {
        BuilderError::Cancelled(message) => ExecutionError::Cancelled(message),
        other => ExecutionError::Build(other.to_string()),
    }
}

pub(crate) fn map_store_error(error: StoreError) -> ExecutionError {
    ExecutionError::Store(error.to_string())
}

pub(crate) fn log_execution_event(
    logger: &dyn BuildLogger,
    level: BuildLogLevel,
    status: BuildStatus,
    message: impl Into<String>,
) {
    logger.log_event(BuildLogEvent {
        level,
        status,
        op: None,
        message: message.into(),
        object_hash: None,
        raw_log_path: None,
        details: serde_json::Map::new(),
    });
}

pub(crate) fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ExecutionError> {
    if cancellation.is_cancelled() {
        Err(ExecutionError::Cancelled(
            "build cancelled by signal".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn cleanup_temp_dir(temp_dir: &StoreTempDir, logger: &dyn BuildLogger) {
    if let Err(error) = temp_dir.remove_force() {
        log_execution_event(
            logger,
            BuildLogLevel::Warn,
            BuildStatus::Cleanup,
            format!(
                "failed to remove temp dir '{}': {error}",
                temp_dir.path().display()
            ),
        );
    }
}

/// RAII owner of a node's temp directory.
///
/// Created right after `create_workspace` (before the node logger exists), so
/// that every post-workspace exit path — error, success, or panic-unwind —
/// cleans the temp dir exactly once via `Drop`. Cache hits return before a
/// workspace exists, so they do not need a temp guard. Until a node logger is
/// attached with [`TempDirGuard::set_logger`], cleanup warnings go to a no-op
/// logger.
pub(crate) struct TempDirGuard {
    temp_dir: StoreTempDir,
    logger: Arc<dyn BuildLogger>,
}

impl TempDirGuard {
    pub(crate) fn for_builder(temp_dir: StoreTempDir) -> Self {
        Self {
            temp_dir,
            logger: Arc::new(NoopBuildLogger),
        }
    }

    pub(crate) fn for_source(temp_dir: StoreTempDir) -> Self {
        Self {
            temp_dir,
            logger: Arc::new(NoopBuildLogger),
        }
    }

    pub(crate) fn set_logger(&mut self, logger: Arc<dyn BuildLogger>) {
        self.logger = logger;
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        cleanup_temp_dir(&self.temp_dir, self.logger.as_ref());
    }
}

pub fn execute_request(request_path: &Path) -> Result<RealizedObject, ExecutionError> {
    if !request_path.exists() {
        return Err(ExecutionError::RequestLoad(format!(
            "request file '{}' does not exist",
            request_path.display()
        )));
    }

    let request_bytes = fs::read(request_path).map_err(|error| {
        ExecutionError::RequestLoad(format!(
            "failed to read request file '{}': {error}",
            request_path.display()
        ))
    })?;
    let request = Request::parse_json(&request_bytes)?;
    execute(request, CancellationToken::new())
}

pub fn execute(
    request: Request,
    cancellation: CancellationToken,
) -> Result<RealizedObject, ExecutionError> {
    let Request {
        store,
        quiet,
        jobs,
        nodes,
        ..
    } = request;
    let jobs = jobs.unwrap_or_else(default_jobs);
    if jobs == 0 {
        return Err(ExecutionError::InvalidRequest(
            "request 'jobs' must be greater than zero".to_string(),
        ));
    }
    let quiet = quiet.unwrap_or(false);
    check_cancelled(&cancellation)?;

    let builder_registry = create_builder_registry()?;
    let mut subjects = HashMap::new();
    let root_key = collect_graph(&nodes, &builder_registry, &mut subjects)?;

    let store = Store::create(&store).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> =
        Arc::new(build_run_logger_for_store(&store, quiet).map_err(ExecutionError::Store)?);
    let runtime_provider = runtime_provider_for_current_process();

    execute_graph(
        &store,
        logger,
        runtime_provider,
        &subjects,
        root_key,
        jobs,
        cancellation,
    )
}

pub fn render_object_as_json(object: &RealizedObject) -> Result<String, ExecutionError> {
    let mut rendered = to_string_pretty(object).map_err(|error| {
        ExecutionError::InvalidRequest(format!("failed to render realized object as JSON: {error}"))
    })?;
    rendered.push('\n');
    Ok(rendered)
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn completed_inputs_for_builder(
    completed: &HashMap<BuildKey, RealizedObject>,
    subjects: &SubjectGraph,
    builder: &BuilderPlannedSubject,
) -> Result<HashMap<BuildKey, RealizedInput>, ExecutionError> {
    let mut realized_inputs = HashMap::new();
    for dep in builder.inputs().values() {
        let realized = completed.get(dep).cloned().ok_or_else(|| {
            ExecutionError::Build(format!(
                "dependency object '{}' is not available in completed set",
                dep
            ))
        })?;
        let subject = subjects.get(dep).ok_or_else(|| {
            ExecutionError::Build(format!(
                "dependency subject '{}' is not available in planned graph",
                dep
            ))
        })?;
        realized_inputs.insert(
            *dep,
            RealizedInput {
                realized,
                materialization_name: subject.name().to_string(),
            },
        );
    }
    Ok(realized_inputs)
}

fn execute_graph(
    store: &Store,
    logger: Arc<BuildRunLogger>,
    runtime_provider: RuntimeProvider,
    subjects: &SubjectGraph,
    root_key: BuildKey,
    jobs: usize,
    cancellation: CancellationToken,
) -> Result<RealizedObject, ExecutionError> {
    let mut counters = RunCounters::default();
    log_run_started(logger.as_ref(), subjects, root_key, jobs, &runtime_provider);

    // Plan top-down: classify nodes lazily, pruning cached subtrees. A node
    // whose output is reachable by build key alone (build-handle hit) is marked
    // Reused, and its dependencies are never visited.
    let mut states = PlanningStates::new();
    if let Err(error) = resolve_planning_state(store, subjects, &mut states, root_key) {
        let result = Err(error);
        log_run_finished(logger.as_ref(), &result, &counters);
        return result;
    }

    // Seed reused objects and surface them on the run-level channel. Only the
    // resolved boundary is recorded; interior nodes of a cached subtree were
    // never visited, so they are neither resolved nor logged.
    let mut completed = HashMap::<BuildKey, RealizedObject>::new();
    for (key, state) in &states {
        if let PlanningState::Reused { realized } = state {
            completed.insert(*key, realized.clone());
            counters.cache_hit += 1;
            log_cache_hit(logger.as_ref(), subjects, *key, realized.object_hash);
        }
    }

    // A reused root means there are no misses to build.
    if let Some(realized) = completed.get(&root_key).cloned() {
        let result = Ok(realized);
        log_run_finished(logger.as_ref(), &result, &counters);
        return result;
    }

    // Build the dependency in-degree over the miss frontier only; reused inputs
    // are already in `completed`, so they are not waited on.
    let mut remaining = HashMap::<BuildKey, usize>::new();
    let mut reverse = HashMap::<BuildKey, Vec<BuildKey>>::new();
    let mut ready = VecDeque::<BuildKey>::new();
    let mut first_error: Option<ExecutionError> = None;

    for (key, state) in &states {
        if !matches!(state, PlanningState::NeedsBuild) {
            continue;
        }
        let Some(subject) = subjects.get(key) else {
            first_error = Some(ExecutionError::Store(format!(
                "missing planned subject for key '{key}'"
            )));
            break;
        };
        let mut wait_for = 0usize;
        if let Some(builder) = subject.as_builder() {
            for dep in builder.inputs().values() {
                if matches!(states.get(dep), Some(PlanningState::NeedsBuild)) {
                    wait_for += 1;
                    reverse.entry(*dep).or_default().push(*key);
                }
            }
        }
        remaining.insert(*key, wait_for);
        if wait_for == 0 {
            ready.push_back(*key);
        }
    }

    let (tx, rx) = mpsc::channel::<(BuildKey, Result<SubjectExecution, ExecutionError>)>();
    let mut in_flight = 0usize;

    // Workers run on detached threads tracked only by `in_flight`. Once a
    // worker is spawned, scheduler-side failures should be recorded in
    // `first_error` so the loop stops launching new work and drains already
    // running workers before returning. The channel-disconnected case below is
    // different: it means no sender remains, so no worker can report a result.
    while !completed.contains_key(&root_key) {
        if first_error.is_none() && cancellation.is_cancelled() {
            first_error = Some(ExecutionError::Cancelled(
                "build cancelled by signal".to_string(),
            ));
        }

        while first_error.is_none() && !cancellation.is_cancelled() && in_flight < jobs {
            let Some(key) = ready.pop_front() else {
                break;
            };
            let subject = match subjects.get(&key) {
                Some(subject) => subject.clone(),
                None => {
                    first_error = Some(ExecutionError::Store(format!(
                        "missing planned subject for key '{}'",
                        key
                    )));
                    break;
                }
            };
            let store = store.clone();
            let logger = logger.clone();
            let runtime_provider = runtime_provider.clone();
            let tx = tx.clone();
            let cancellation = cancellation.clone();
            let realized_inputs = match subject.as_builder() {
                Some(builder) => {
                    match completed_inputs_for_builder(&completed, subjects, builder) {
                        Ok(realized_inputs) => realized_inputs,
                        Err(error) => {
                            first_error = Some(error);
                            break;
                        }
                    }
                }
                None => HashMap::new(),
            };
            thread::spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    execute_subject(
                        &subject,
                        PlannedExecutionContext {
                            store: &store,
                            run_logger: logger,
                            runtime_provider,
                            cancellation,
                            realized_inputs: &realized_inputs,
                        },
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(ExecutionError::Build(format!(
                        "worker thread for key '{key}' panicked"
                    )))
                });
                let _ = tx.send((key, result));
            });
            in_flight += 1;
        }

        if completed.contains_key(&root_key) {
            break;
        }

        if in_flight == 0 {
            // No ready work and nothing running: if no failure was recorded,
            // this is a stall. Funnel both cases through the single terminal
            // exit below so the run-finished event is always emitted.
            if first_error.is_none() {
                first_error = Some(ExecutionError::Build(
                    "planner/executor stalled: no ready jobs and root object is still unresolved"
                        .to_string(),
                ));
            }
            break;
        }

        // Channel disconnect is unreachable while this function holds its own
        // `tx`, but treat it as a recorded error and break so the terminal exit
        // still emits the run-finished event.
        let (key, result) = match rx.recv() {
            Ok(received) => received,
            Err(_) => {
                if first_error.is_none() {
                    first_error = Some(ExecutionError::Build(
                        "worker channel closed unexpectedly".to_string(),
                    ));
                }
                break;
            }
        };
        in_flight -= 1;
        let executed = match result {
            Ok(executed) => executed,
            Err(error) => {
                counters.failed += 1;
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
        };
        let object_hash = executed.realized.object_hash;
        match executed.outcome {
            SubjectOutcome::CacheHit => {
                // Cache hits have no per-subject logger; surface them on the
                // run-level channel so a fully cached run still leaves an audit
                // trail.
                counters.cache_hit += 1;
                log_cache_hit(logger.as_ref(), subjects, key, object_hash);
            }
            SubjectOutcome::Built => {
                counters.built += 1;
                executed.logger.log_event(BuildLogEvent {
                    level: BuildLogLevel::Info,
                    status: BuildStatus::Done,
                    op: None,
                    message: "subject completed".to_string(),
                    object_hash: Some(object_hash),
                    raw_log_path: None,
                    details: serde_json::Map::new(),
                });
            }
        }
        completed.insert(key, executed.realized);
        if first_error.is_none()
            && let Some(parents) = reverse.get(&key)
        {
            for parent in parents {
                let Some(pending) = remaining.get_mut(parent) else {
                    first_error.get_or_insert(ExecutionError::Store(format!(
                        "missing pending-dependency counter for key '{}'",
                        parent
                    )));
                    break;
                };
                *pending -= 1;
                if *pending == 0 {
                    ready.push_back(*parent);
                }
            }
        }
    }

    let result = if let Some(error) = first_error {
        Err(error)
    } else {
        completed.get(&root_key).cloned().ok_or_else(|| {
            ExecutionError::Store(format!(
                "root object for key '{}' is missing after executor completion",
                root_key
            ))
        })
    };
    log_run_finished(logger.as_ref(), &result, &counters);
    result
}

/// Outcome of planning one subject: either reusable as-is or to be built.
enum PlanningState {
    /// Reachable from the store by build key alone (build-handle hit). Its
    /// dependency subtree is never visited.
    Reused { realized: RealizedObject },
    /// Must be built (or resolved at runtime by execute_subject).
    NeedsBuild,
}

type PlanningStates = HashMap<BuildKey, PlanningState>;

/// Classifies subjects lazily, top-down, pruning cached subtrees.
///
/// A direct build-handle lookup needs no inputs, so a hit marks the node Reused
/// and returns without descending — the dependencies of a cached object never
/// have to be resolved. On a miss, only the node's immediate inputs are
/// visited. Canonical (reuse-key) and source existing-object resolution are not
/// done here; they stay in execute_subject, which runs after a node's inputs
/// are built and their object hashes are known.
fn resolve_planning_state(
    store: &Store,
    subjects: &SubjectGraph,
    states: &mut PlanningStates,
    key: BuildKey,
) -> Result<(), ExecutionError> {
    if states.contains_key(&key) {
        return Ok(());
    }
    let subject = subjects
        .get(&key)
        .ok_or_else(|| ExecutionError::Store(format!("missing planned subject for key '{key}'")))?
        .clone();

    // Pure lookup (no object-ref update): reused subtrees are left untouched.
    if let Some(published) = load_build_handle(store, key).map_err(map_store_error)? {
        states.insert(
            key,
            PlanningState::Reused {
                realized: realized_object_from_record(Some(key), &published.object_record),
            },
        );
        return Ok(());
    }

    let input_keys: Vec<BuildKey> = subject
        .as_builder()
        .map(|builder| builder.inputs().values().copied().collect())
        .unwrap_or_default();
    for dep in input_keys {
        resolve_planning_state(store, subjects, states, dep)?;
    }
    states.insert(key, PlanningState::NeedsBuild);
    Ok(())
}

#[derive(Default)]
struct RunCounters {
    built: usize,
    cache_hit: usize,
    failed: usize,
}

fn backend_label(provider: &RuntimeProvider) -> &'static str {
    match provider.backend() {
        RuntimeBackend::Host => "host",
        RuntimeBackend::Namespace => "namespace",
    }
}

fn json_object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

fn log_run_started(
    logger: &BuildRunLogger,
    subjects: &SubjectGraph,
    root_key: BuildKey,
    jobs: usize,
    provider: &RuntimeProvider,
) {
    let root_details = subjects.get(&root_key).map(|subject| {
        let tag = subject
            .as_builder()
            .map_or("Source", |builder| builder.tag());
        serde_json::json!({
            "tag": tag,
            "name": subject.name(),
            "build_key": root_key.to_string(),
        })
    });
    logger.log_run_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        status: BuildStatus::RunStarted,
        op: None,
        message: "build started".to_string(),
        object_hash: None,
        raw_log_path: None,
        details: json_object(serde_json::json!({
            "jobs": jobs,
            "subjects": subjects.len(),
            "backend": backend_label(provider),
            "root": root_details,
        })),
    });
}

fn log_cache_hit(
    logger: &BuildRunLogger,
    subjects: &SubjectGraph,
    key: BuildKey,
    object_hash: ObjectHash,
) {
    let Some(subject) = subjects.get(&key) else {
        return;
    };
    let tag = subject
        .as_builder()
        .map_or("Source", |builder| builder.tag());
    let identity = SubjectIdentity::new(tag, subject.name(), key.to_string());
    logger.log_subject_event(
        &identity,
        BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::CacheHit,
            op: None,
            message: "served from cache".to_string(),
            object_hash: Some(object_hash),
            raw_log_path: None,
            details: serde_json::Map::new(),
        },
    );
}

fn log_run_finished(
    logger: &BuildRunLogger,
    result: &Result<RealizedObject, ExecutionError>,
    counters: &RunCounters,
) {
    let (level, object_hash, outcome, message) = match result {
        Ok(realized) => (
            BuildLogLevel::Info,
            Some(realized.object_hash),
            "ok",
            "build finished".to_string(),
        ),
        Err(error) if error.class() == "cancelled" => (
            BuildLogLevel::Error,
            None,
            "cancelled",
            error.message().to_string(),
        ),
        Err(error) => (
            BuildLogLevel::Error,
            None,
            "failed",
            error.message().to_string(),
        ),
    };
    logger.log_run_event(BuildLogEvent {
        level,
        status: BuildStatus::RunFinished,
        op: None,
        message,
        object_hash,
        raw_log_path: None,
        details: json_object(serde_json::json!({
            "result": outcome,
            "built": counters.built,
            "cache_hit": counters.cache_hit,
            "failed": counters.failed,
        })),
    });
}

fn build_run_logger_for_store(store: &Store, quiet: bool) -> Result<BuildRunLogger, String> {
    let locations = store.run_log_locations();
    BuildRunLogger::new(locations.run_log_dir(), locations.run_id(), quiet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::{BuildLogSubject, compute_build_key, compute_reuse_key};
    use bobr_store::{
        StoreWorkspace, create_workspace, materialize_build, resolve_reuse_for_build,
    };
    use mbuild_builder::{
        BuildContext, BuilderInputs, BuilderRegistry, InputSpec, StagedBuildResult, TypedBuilder,
    };
    use mbuild_source::{OriginContext, OriginSpec, ParsedOrigin, SourcePlannedSubject};
    use serde::Deserialize;
    use serde_json::{Map, Value, json};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn create_test_store(root: &Path) -> Store {
        let store_root = root.join(".mbuild");
        fs::create_dir_all(&store_root).unwrap();
        Store::create(&store_root).unwrap()
    }

    fn create_test_logger(store: &Store) -> Arc<BuildRunLogger> {
        Arc::new(build_run_logger_for_store(store, true).unwrap())
    }

    fn create_test_run(
        store: &Store,
        run_logger: &Arc<BuildRunLogger>,
        tag: &str,
        name: &str,
        build_key: BuildKey,
    ) -> (StoreWorkspace, Arc<dyn BuildLogger>) {
        let workspace = create_workspace(store, tag, name, build_key.to_string()).unwrap();
        let subject = BuildLogSubject::new(
            tag,
            name,
            build_key.to_string(),
            workspace.log_dir().to_path_buf(),
            workspace.raw_log_dir().to_path_buf(),
        );
        let logger = run_logger.bind_subject(subject).unwrap();
        (workspace, logger)
    }

    fn run_builder_subject(
        store: &Store,
        run_logger: Arc<BuildRunLogger>,
        builder: &'static dyn mbuild_builder::Builder,
        name: &str,
        config: Value,
        cancellation: CancellationToken,
    ) -> Result<SubjectExecution, ExecutionError> {
        let mut registry = BuilderRegistry::new();
        registry.register(builder).unwrap();
        let subject = registry
            .parse_subject(
                builder.tag(),
                json!({"name": name, "config": config})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap();
        let realized_inputs = HashMap::new();
        execute_subject(
            &PlannedSubject::Builder(subject),
            PlannedExecutionContext {
                store,
                run_logger,
                runtime_provider: RuntimeProvider::host(),
                cancellation,
                realized_inputs: &realized_inputs,
            },
        )
    }

    fn workspace_metadata(root: &Path, tag: &str, name: &str) -> Value {
        let logs_root = root.join(".mbuild").join("logs");
        let mut run_dirs = fs::read_dir(&logs_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        run_dirs.sort();
        for run_dir in run_dirs {
            for entry in fs::read_dir(run_dir).unwrap() {
                let path = entry.unwrap().path();
                let meta_path = path.join("meta.json");
                if !meta_path.is_file() {
                    continue;
                }
                let metadata: Value =
                    serde_json::from_slice(&fs::read(meta_path).unwrap()).unwrap();
                if metadata["tag"] == tag && metadata["name"] == name {
                    return metadata;
                }
            }
        }
        panic!("missing workspace metadata for {tag}/{name}");
    }

    fn metadata_temp_dir(metadata: &Value) -> PathBuf {
        PathBuf::from(metadata["temp_dir"].as_str().unwrap())
    }

    fn metadata_log_dir(metadata: &Value) -> PathBuf {
        PathBuf::from(metadata["log_dir"].as_str().unwrap())
    }

    fn event_log_records(log_dir: &Path) -> Vec<Value> {
        fs::read_to_string(log_dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_cleanup_warning_event(
        log_dir: &Path,
        builder: &str,
        name: &str,
        build_key: BuildKey,
        message_fragment: &str,
    ) -> Value {
        let events = event_log_records(log_dir)
            .into_iter()
            .filter(|event| event["status"] == "cleanup")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);

        let event = events.into_iter().next().unwrap();
        assert_eq!(event["level"], "warn");
        assert_eq!(event["subject"]["tag"], builder);
        assert_eq!(event["subject"]["name"], name);
        assert_eq!(event["subject"]["build_key"], build_key.to_string());

        let message = event["message"].as_str().unwrap();
        assert!(message.contains(message_fragment));
        event
    }

    fn workspace_count(root: &Path) -> usize {
        let logs_root = root.join(".mbuild").join("logs");
        fs::read_dir(&logs_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .flat_map(|run_dir| fs::read_dir(run_dir).unwrap())
            .filter(|entry| entry.as_ref().unwrap().path().join("meta.json").is_file())
            .count()
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ExecutionTestConfig {}

    static EXECUTION_TEST_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    #[derive(Debug)]
    struct ExecutionTestBuilder;
    static EXECUTION_TEST_BUILDER: ExecutionTestBuilder = ExecutionTestBuilder;

    impl TypedBuilder for ExecutionTestBuilder {
        type Config = ExecutionTestConfig;

        fn tag(&self) -> &'static str {
            "ExecutionTest"
        }

        fn spec(&self) -> &'static InputSpec {
            &EXECUTION_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_builder::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);

            fs::create_dir_all(cx.temp_dir.join("out")).unwrap();
            fs::write(cx.temp_dir.join("out").join("payload"), b"ok\n").unwrap();

            Ok(StagedBuildResult {
                staged_path: cx.temp_dir.join("out"),
            })
        }
    }

    static SANDBOX_EXECUTION_TEST_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    #[derive(Debug)]
    struct SandboxExecutionTestBuilder;
    static SANDBOX_EXECUTION_TEST_BUILDER: SandboxExecutionTestBuilder =
        SandboxExecutionTestBuilder;

    impl TypedBuilder for SandboxExecutionTestBuilder {
        type Config = ExecutionTestConfig;

        fn tag(&self) -> &'static str {
            "Sandbox"
        }

        fn spec(&self) -> &'static InputSpec {
            &SANDBOX_EXECUTION_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_builder::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);

            fs::create_dir_all(cx.temp_dir.join("out")).unwrap();
            fs::write(cx.temp_dir.join("out").join("payload"), b"ok\n").unwrap();
            fs::write(cx.temp_dir.join("sandbox-scratch"), b"sandbox scratch\n").unwrap();

            Ok(StagedBuildResult {
                staged_path: cx.temp_dir.join("out"),
            })
        }
    }

    #[derive(Debug)]
    struct ExecutionFailBuilder;
    static EXECUTION_FAIL_BUILDER: ExecutionFailBuilder = ExecutionFailBuilder;

    impl TypedBuilder for ExecutionFailBuilder {
        type Config = ExecutionTestConfig;

        fn tag(&self) -> &'static str {
            "ExecutionTest"
        }

        fn spec(&self) -> &'static InputSpec {
            &EXECUTION_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_builder::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);
            fs::write(cx.temp_dir.join("scratch"), b"temp\n").unwrap();
            Err(mbuild_builder::BuilderError::ExecutionFailed(
                "intentional failure".to_string(),
            ))
        }
    }

    #[derive(Debug)]
    struct ExecutionBrokenStageBuilder;
    static EXECUTION_BROKEN_STAGE_BUILDER: ExecutionBrokenStageBuilder =
        ExecutionBrokenStageBuilder;

    impl TypedBuilder for ExecutionBrokenStageBuilder {
        type Config = ExecutionTestConfig;

        fn tag(&self) -> &'static str {
            "ExecutionTest"
        }

        fn spec(&self) -> &'static InputSpec {
            &EXECUTION_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_builder::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);
            fs::write(cx.temp_dir.join("scratch"), b"temp\n").unwrap();

            Ok(StagedBuildResult {
                staged_path: cx.temp_dir.join("missing-output"),
            })
        }
    }

    #[test]
    fn resolve_reuse_for_build_depends_on_input_object_hash() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let matching_inputs = vec![
            "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
        ];
        let payload = json!({ "source": "echo hi\n", "executable": true });
        let reuse_key =
            compute_reuse_key("ExecutionLookupTest", &payload, &matching_inputs).unwrap();
        let build_key_for_object =
            BuildKey::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .unwrap();
        let lookup_build_key =
            BuildKey::from_str("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
                .unwrap();
        let stage = temp.path().join("script.sh");
        fs::write(&stage, b"echo hi\n").unwrap();
        materialize_build(
            &store,
            build_key_for_object,
            reuse_key,
            matching_inputs.clone(),
            &stage,
            Some("script"),
        )
        .unwrap();

        let matching_reuse_key =
            compute_reuse_key("ExecutionLookupTest", &payload, &matching_inputs).unwrap();
        let hit =
            resolve_reuse_for_build(&store, lookup_build_key, matching_reuse_key, Some("script"))
                .unwrap()
                .expect("expected canonical object hit");
        assert_eq!(hit.build.build_key, lookup_build_key);

        let mismatching_inputs = vec![
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .parse()
                .unwrap(),
        ];
        let mismatching_reuse_key =
            compute_reuse_key("ExecutionLookupTest", &payload, &mismatching_inputs).unwrap();
        assert!(
            resolve_reuse_for_build(
                &store,
                lookup_build_key,
                mismatching_reuse_key,
                Some("script")
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn execute_builder_node_prepares_dirs_and_cleans_temp_on_success() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let executed = run_builder_subject(
            &store,
            logger,
            &EXECUTION_TEST_BUILDER,
            "runtime-test",
            json!({}),
            CancellationToken::new(),
        )
        .unwrap();

        let metadata = workspace_metadata(temp.path(), "ExecutionTest", "runtime-test");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
        let object_path = store.object_path(executed.realized.object_hash);
        assert!(object_path.is_dir());
        assert!(object_path.join("payload").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_temp_reports_stale_temp_recreate_failure() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let config = json!({});
        let build_key = compute_build_key("ExecutionTest", &config, &[]).unwrap();
        let run_logger = create_test_logger(&store);
        let (workspace, _logger) = create_test_run(
            &store,
            &run_logger,
            "ExecutionTest",
            "runtime-test",
            build_key,
        );
        let temp_dir = workspace.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        let stale_target = temp.path().join("missing-stale-target");
        symlink(&stale_target, &temp_dir).unwrap();

        let error = prepare_temp(workspace.temp_dir_handle()).unwrap_err();

        assert_eq!(error.class(), "store");
        assert!(error.message().contains("failed to create directory"));
        assert!(
            fs::symlink_metadata(&temp_dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(event_log_records(workspace.log_dir()).is_empty());
    }

    #[test]
    fn execute_sandbox_builder_removes_temp_on_success() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);

        let executed = run_builder_subject(
            &store,
            logger,
            &SANDBOX_EXECUTION_TEST_BUILDER,
            "sandbox-runtime-test",
            json!({}),
            CancellationToken::new(),
        )
        .unwrap();

        let metadata = workspace_metadata(temp.path(), "Sandbox", "sandbox-runtime-test");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
        let object_path = store.object_path(executed.realized.object_hash);
        assert!(object_path.join("payload").is_file());
        assert!(
            event_log_records(&metadata_log_dir(&metadata))
                .iter()
                .all(|event| event["status"] != "cleanup")
        );
    }

    #[test]
    fn prepare_temp_removes_stale_sandbox_temp_before_recreate() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let config = json!({});
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();
        let (workspace, _logger) = create_test_run(
            &store,
            &run_logger,
            "Sandbox",
            "sandbox-runtime-test",
            build_key,
        );
        let temp_dir = workspace.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("stale"), b"old\n").unwrap();

        prepare_temp(workspace.temp_dir_handle()).unwrap();

        assert!(temp_dir.is_dir());
        assert_eq!(fs::read_dir(&temp_dir).unwrap().count(), 0);
        assert!(event_log_records(workspace.log_dir()).is_empty());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_failure() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let error = run_builder_subject(
            &store,
            logger,
            &EXECUTION_FAIL_BUILDER,
            "runtime-fail",
            Value::Object(Map::new()),
            CancellationToken::new(),
        )
        .unwrap_err();

        assert_eq!(error.class(), "build");
        let metadata = workspace_metadata(temp.path(), "ExecutionTest", "runtime-fail");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_temp_dir_warns_when_remove_fails() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let build_key = compute_build_key("ExecutionTest", &json!({}), &[]).unwrap();
        let (workspace, logger) = create_test_run(
            &store,
            &run_logger,
            "ExecutionTest",
            "runtime-test",
            build_key,
        );
        let temp_dir = workspace.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        fs::write(&temp_dir, b"not a directory\n").unwrap();

        cleanup_temp_dir(workspace.temp_dir_handle(), logger.as_ref());

        assert!(temp_dir.is_file());
        assert_cleanup_warning_event(
            workspace.log_dir(),
            "ExecutionTest",
            "runtime-test",
            build_key,
            "failed to remove temp dir",
        );
    }

    #[test]
    fn temp_dir_guard_cleans_without_a_node_logger() {
        // Simulates the pre-logger window: a failure (e.g. bind_subject) before
        // a node logger is attached. The guard, created right after the
        // workspace, must still clean the temp dir on drop via the no-op logger.
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let build_key = compute_build_key("ExecutionTest", &json!({}), &[]).unwrap();
        let (workspace, _logger) = create_test_run(
            &store,
            &run_logger,
            "ExecutionTest",
            "guard-test",
            build_key,
        );
        let temp_dir = workspace.temp_dir().to_path_buf();
        assert!(temp_dir.is_dir());

        {
            // No set_logger call: the node logger was never bound.
            let _guard = TempDirGuard::for_builder(workspace.temp_dir_handle().clone());
        }

        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_materialize_failure() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let error = run_builder_subject(
            &store,
            logger,
            &EXECUTION_BROKEN_STAGE_BUILDER,
            "runtime-broken-stage",
            json!({}),
            CancellationToken::new(),
        )
        .unwrap_err();

        assert_eq!(error.class(), "store");
        let metadata = workspace_metadata(temp.path(), "ExecutionTest", "runtime-broken-stage");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_builder_node_pre_cancelled_does_not_start_builder() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = run_builder_subject(
            &store,
            logger,
            &EXECUTION_TEST_BUILDER,
            "runtime-test",
            json!({}),
            cancellation,
        )
        .unwrap_err();

        assert_eq!(error.class(), "cancelled");
        assert_eq!(workspace_count(temp.path()), 0);
    }

    #[derive(Debug, Clone)]
    struct CancellingOrigin {
        cancellation: CancellationToken,
    }

    impl ParsedOrigin for CancellingOrigin {
        fn spec(&self) -> &'static OriginSpec {
            static SPEC: OriginSpec = OriginSpec {
                tag: "cancelling-test",
            };
            &SPEC
        }

        fn materialize(&self, cx: &OriginContext<'_>) -> Result<std::path::PathBuf, String> {
            let staged = cx.temp_root.join("staged");
            fs::create_dir_all(&staged).map_err(|error| error.to_string())?;
            fs::write(staged.join("payload"), b"cancel\n").map_err(|error| error.to_string())?;
            self.cancellation.cancel();
            Ok(staged)
        }

        fn clone_box(&self) -> Box<dyn ParsedOrigin> {
            Box::new(self.clone())
        }
    }

    #[derive(Debug, Clone)]
    struct EscapingOrigin;

    impl ParsedOrigin for EscapingOrigin {
        fn spec(&self) -> &'static OriginSpec {
            static SPEC: OriginSpec = OriginSpec {
                tag: "escaping-test",
            };
            &SPEC
        }

        fn materialize(&self, cx: &OriginContext<'_>) -> Result<std::path::PathBuf, String> {
            let staged = cx
                .temp_root
                .parent()
                .ok_or_else(|| "source temp root has no parent".to_string())?
                .join("escaped-staged");
            fs::create_dir_all(&staged).map_err(|error| error.to_string())?;
            fs::write(staged.join("payload"), b"escaped\n").map_err(|error| error.to_string())?;
            Ok(staged)
        }

        fn clone_box(&self) -> Box<dyn ParsedOrigin> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn source_temp_dir_is_removed_when_cancelled_after_materialize() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let cancellation = CancellationToken::new();
        let object_hash = "1111111111111111111111111111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let subject = PlannedSubject::Source(SourcePlannedSubject::new(
            "cancel-source".to_string(),
            object_hash,
            Some(Box::new(CancellingOrigin {
                cancellation: cancellation.clone(),
            })),
        ));

        let realized_inputs = HashMap::new();
        let error = execute_subject(
            &subject,
            PlannedExecutionContext {
                store: &store,
                run_logger: logger,
                runtime_provider: RuntimeProvider::host(),
                cancellation,
                realized_inputs: &realized_inputs,
            },
        )
        .expect_err("expected cancellation");

        assert_eq!(error.class(), "cancelled");
        let metadata = workspace_metadata(temp.path(), "Source", "cancel-source");
        let temp_dir = PathBuf::from(metadata["temp_dir"].as_str().unwrap());
        assert!(!temp_dir.exists());
    }

    #[test]
    fn source_origin_staged_path_must_remain_under_temp_root() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let object_hash = "3333333333333333333333333333333333333333333333333333333333333333"
            .parse()
            .unwrap();
        let subject = PlannedSubject::Source(SourcePlannedSubject::new(
            "escaping-source".to_string(),
            object_hash,
            Some(Box::new(EscapingOrigin)),
        ));

        let realized_inputs = HashMap::new();
        let error = execute_subject(
            &subject,
            PlannedExecutionContext {
                store: &store,
                run_logger: logger,
                runtime_provider: RuntimeProvider::host(),
                cancellation: CancellationToken::new(),
                realized_inputs: &realized_inputs,
            },
        )
        .expect_err("expected escaping origin to be rejected");

        assert_eq!(error.class(), "build");
        assert!(error.to_string().contains("outside temp root"), "{error}");
        let metadata = workspace_metadata(temp.path(), "Source", "escaping-source");
        let temp_dir = PathBuf::from(metadata["temp_dir"].as_str().unwrap());
        assert!(!temp_dir.exists());
    }

    #[test]
    fn source_temp_dir_is_removed_when_origin_missing() {
        // No origin and the object is not in the store: execute_source_subject
        // returns a Build error after the workspace is created. The temp dir
        // must still be cleaned — i.e. the RAII guard cleans this error path.
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let object_hash = "2222222222222222222222222222222222222222222222222222222222222222"
            .parse()
            .unwrap();
        let subject = PlannedSubject::Source(SourcePlannedSubject::new(
            "no-origin-source".to_string(),
            object_hash,
            None,
        ));

        let realized_inputs = HashMap::new();
        let error = execute_subject(
            &subject,
            PlannedExecutionContext {
                store: &store,
                run_logger: logger,
                runtime_provider: RuntimeProvider::host(),
                cancellation: CancellationToken::new(),
                realized_inputs: &realized_inputs,
            },
        )
        .expect_err("expected build error for missing origin");

        assert_eq!(error.class(), "build");
        let metadata = workspace_metadata(temp.path(), "Source", "no-origin-source");
        let temp_dir = PathBuf::from(metadata["temp_dir"].as_str().unwrap());
        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_graph_drains_in_flight_workers_when_publish_fails() {
        use mbuild_builder::InputSlot;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // Set when the slow worker finishes; lets us prove execute_graph waited
        // for it (drained) before returning the fast node's publish error.
        static SLOW_FINISHED: AtomicBool = AtomicBool::new(false);

        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EmptyConfig {}

        static DRAIN_SPEC: InputSpec = InputSpec {
            required_inputs: &[],
            optional_inputs: &[],
            allow_extra_inputs: false,
        };

        fn stage_payload(cx: &mut BuildContext, body: &[u8]) -> StagedBuildResult {
            fs::create_dir_all(cx.temp_dir.join("out")).unwrap();
            fs::write(cx.temp_dir.join("out").join("payload"), body).unwrap();
            StagedBuildResult {
                staged_path: cx.temp_dir.join("out"),
            }
        }

        #[derive(Debug)]
        struct FastBuilder;
        static FAST_BUILDER: FastBuilder = FastBuilder;
        impl TypedBuilder for FastBuilder {
            type Config = EmptyConfig;
            fn tag(&self) -> &'static str {
                "DrainFast"
            }
            fn spec(&self) -> &'static InputSpec {
                &DRAIN_SPEC
            }
            fn build_typed(
                &self,
                _config: Self::Config,
                _inputs: BuilderInputs,
                cx: &mut BuildContext,
            ) -> Result<StagedBuildResult, mbuild_builder::BuilderError> {
                Ok(stage_payload(cx, b"fast\n"))
            }
        }

        #[derive(Debug)]
        struct SlowBuilder;
        static SLOW_BUILDER: SlowBuilder = SlowBuilder;
        impl TypedBuilder for SlowBuilder {
            type Config = EmptyConfig;
            fn tag(&self) -> &'static str {
                "DrainSlow"
            }
            fn spec(&self) -> &'static InputSpec {
                &DRAIN_SPEC
            }
            fn build_typed(
                &self,
                _config: Self::Config,
                _inputs: BuilderInputs,
                cx: &mut BuildContext,
            ) -> Result<StagedBuildResult, mbuild_builder::BuilderError> {
                std::thread::sleep(Duration::from_millis(300));
                SLOW_FINISHED.store(true, Ordering::SeqCst);
                Ok(stage_payload(cx, b"slow\n"))
            }
        }

        // Root depends on both leaves so that lazy planning schedules them:
        // the planner only visits the root's dependency closure, so the slow
        // node must be reachable from the root to be in flight when fast fails.
        #[derive(Debug)]
        struct RootBuilder;
        static ROOT_BUILDER: RootBuilder = RootBuilder;
        static DRAIN_ROOT_SPEC: InputSpec = InputSpec {
            required_inputs: &[InputSlot::object("fast"), InputSlot::object("slow")],
            optional_inputs: &[],
            allow_extra_inputs: false,
        };
        impl TypedBuilder for RootBuilder {
            type Config = EmptyConfig;
            fn tag(&self) -> &'static str {
                "DrainRoot"
            }
            fn spec(&self) -> &'static InputSpec {
                &DRAIN_ROOT_SPEC
            }
            fn build_typed(
                &self,
                _config: Self::Config,
                _inputs: BuilderInputs,
                cx: &mut BuildContext,
            ) -> Result<StagedBuildResult, mbuild_builder::BuilderError> {
                // Never reached: the fast input fails to publish first.
                Ok(stage_payload(cx, b"root\n"))
            }
        }

        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        fs::create_dir(temp.path().join(".mbuild").join("object-refs").join("bad")).unwrap();

        let mut registry = BuilderRegistry::new();
        registry.register(&FAST_BUILDER).unwrap();
        registry.register(&SLOW_BUILDER).unwrap();
        registry.register(&ROOT_BUILDER).unwrap();

        // Fast node publishes to a pre-existing non-symlink ref path, so its
        // publish fails after the build succeeds. Slow node is a sibling input
        // that is still in flight; both are inputs of the root.
        let fast = registry
            .parse_subject(
                "DrainFast",
                json!({"name": "bad", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap();
        let slow = registry
            .parse_subject(
                "DrainSlow",
                json!({"name": "good", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap();
        let fast_key = fast.build_key();
        let slow_key = slow.build_key();
        let root = registry
            .parse_subject(
                "DrainRoot",
                json!({"name": "root", "config": {}})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::from([
                    ("fast".to_string(), fast_key),
                    ("slow".to_string(), slow_key),
                ]),
            )
            .unwrap();
        let root_key = root.build_key();

        let mut subjects: SubjectGraph = HashMap::new();
        subjects.insert(fast_key, Arc::new(PlannedSubject::Builder(fast)));
        subjects.insert(slow_key, Arc::new(PlannedSubject::Builder(slow)));
        subjects.insert(root_key, Arc::new(PlannedSubject::Builder(root)));

        // The root waits on both leaves, so it never completes once fast's
        // publish fails; the scheduler must drain the in-flight slow worker
        // before returning the error.
        let error = execute_graph(
            &store,
            logger,
            RuntimeProvider::host(),
            &subjects,
            root_key,
            2,
            CancellationToken::new(),
        )
        .expect_err("expected store failure for invalid object ref path");

        assert_eq!(error.class(), "store");
        assert!(
            SLOW_FINISHED.load(Ordering::SeqCst),
            "execute_graph returned before draining the in-flight worker"
        );
    }
}
