use crate::planned::{
    BuilderPlannedSubject, GraphKey, PlannedDependencyLookupContext, PlannedExecutionContext,
    PlannedLookupContext, ReuseOrigin, SubjectExecution,
};
use crate::recipe::{GraphNode, PlanningState, RecipeEnvelope, collect_graph};
use crate::runtime::{RuntimeError, check_cancelled, log_runtime_event, map_store_error};
#[cfg(test)]
use bobr_store::identity::BuildKey;
use bobr_store::{
    RealizedObject, Store, StoreWorkspace, create_workspace, publish_stored_object,
    remove_store_temp_dir_force,
};
use mbuild_core::{
    BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, BuilderRun, CancellationToken,
    Workspace,
};
use serde_json::{Map, Value, to_string_pretty};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub fn run_recipe_json_in_workspace(
    _workspace_root: &Path,
    recipe_path: &Path,
) -> Result<RealizedObject, RuntimeError> {
    if !recipe_path.exists() {
        return Err(RuntimeError::RecipeLoad(format!(
            "recipe file '{}' does not exist",
            recipe_path.display()
        )));
    }

    let recipe_bytes = fs::read(recipe_path).map_err(|error| {
        RuntimeError::RecipeLoad(format!(
            "failed to read recipe file '{}': {error}",
            recipe_path.display()
        ))
    })?;
    let envelope = RecipeEnvelope::parse_json(&recipe_bytes)?;
    run_recipe_envelope(envelope, CancellationToken::new())
}

pub fn run_recipe_envelope(
    envelope: RecipeEnvelope,
    cancellation: CancellationToken,
) -> Result<RealizedObject, RuntimeError> {
    let RecipeEnvelope { options, request } = envelope;
    let jobs = options.jobs.unwrap_or_else(default_jobs);
    if jobs == 0 {
        return Err(RuntimeError::InvalidRequest(
            "recipe options.jobs must be greater than zero".to_string(),
        ));
    }
    let emit_progress = !options.quiet.unwrap_or(false);
    check_cancelled(&cancellation)?;

    let store_path = options.store.as_ref().ok_or_else(|| {
        RuntimeError::InvalidRequest("recipe options.store or --store must be set".to_string())
    })?;
    if store_path.is_absolute() && !store_path.exists() {
        return Err(RuntimeError::Store(format!(
            "store root must exist: '{}'",
            store_path.display()
        )));
    }
    let store = Store::create(store_path).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> =
        Arc::new(build_run_logger_for_store(&store, emit_progress).map_err(RuntimeError::Store)?);

    let mut nodes = HashMap::new();
    let root_key = collect_graph(&request, &mut nodes)?;
    let root_recipe = request.node("root")?;
    let root_name = root_recipe.name().to_string();
    let root_tag = root_recipe.tag().to_string();
    ensure_planned(&store, &mut nodes, root_key)?;

    let mut completed = HashMap::new();
    for (key, node) in &nodes {
        if let PlanningState::Reused { realized, .. } = &node.state {
            completed.insert(*key, realized.clone());
        }
    }

    if let Some(realized) = completed.get(&root_key).cloned() {
        let origin = match &nodes
            .get(&root_key)
            .ok_or_else(|| {
                RuntimeError::Store(format!("missing planned node for key '{}'", root_key))
            })?
            .state
        {
            PlanningState::Reused { origin, .. } => *origin,
            _ => {
                return Err(RuntimeError::Store(format!(
                    "root key '{}' completed without reused state",
                    root_key
                )));
            }
        };
        publish_reused_root(
            &store, &logger, root_key, &root_tag, &root_name, &realized, origin,
        )?;
        return Ok(realized);
    }

    execute_misses(
        &store,
        logger,
        &nodes,
        &mut completed,
        root_key,
        jobs,
        cancellation,
    )
}

pub fn render_object_as_json(object: &RealizedObject) -> Result<String, RuntimeError> {
    let mut rendered = to_string_pretty(object).map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to render realized object as JSON: {error}"))
    })?;
    rendered.push('\n');
    Ok(rendered)
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn ensure_planned(
    store: &Store,
    nodes: &mut HashMap<GraphKey, GraphNode>,
    key: GraphKey,
) -> Result<(), RuntimeError> {
    let subject = {
        let node = nodes.get(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for key '{}'", key))
        })?;
        if !matches!(node.state, PlanningState::Unknown) {
            return Ok(());
        }
        node.subject.clone()
    };

    if let Some(reuse) = subject.lookup_direct_reuse(PlannedLookupContext { store })? {
        set_node_state(
            nodes,
            key,
            PlanningState::Reused {
                realized: reuse.realized,
                origin: reuse.origin,
            },
        )?;
        return Ok(());
    }

    let Some(builder) = subject.as_builder() else {
        set_node_state(nodes, key, PlanningState::NeedsBuild)?;
        return Ok(());
    };

    for dep in builder.inputs().values().copied().collect::<Vec<_>>() {
        ensure_planned(store, nodes, dep)?;
    }

    let Some(realized_inputs) = reused_inputs_for_builder(nodes, builder)? else {
        set_node_state(nodes, key, PlanningState::NeedsBuild)?;
        return Ok(());
    };

    if let Some(reuse) = subject.lookup_after_inputs_reused(PlannedDependencyLookupContext {
        store,
        realized_inputs: &realized_inputs,
    })? {
        set_node_state(
            nodes,
            key,
            PlanningState::Reused {
                realized: reuse.realized,
                origin: reuse.origin,
            },
        )?;
    } else {
        set_node_state(nodes, key, PlanningState::NeedsBuild)?;
    }

    Ok(())
}

fn set_node_state(
    nodes: &mut HashMap<GraphKey, GraphNode>,
    key: GraphKey,
    state: PlanningState,
) -> Result<(), RuntimeError> {
    let node = nodes
        .get_mut(&key)
        .ok_or_else(|| RuntimeError::Store(format!("missing planned node for key '{}'", key)))?;
    node.state = state;
    Ok(())
}

fn reused_inputs_for_builder(
    nodes: &HashMap<GraphKey, GraphNode>,
    builder: &BuilderPlannedSubject,
) -> Result<Option<HashMap<GraphKey, RealizedObject>>, RuntimeError> {
    let mut realized_inputs = HashMap::new();
    for dep in builder.inputs().values() {
        let dep_node = nodes.get(dep).ok_or_else(|| {
            RuntimeError::Store(format!(
                "missing dependency node '{}' for builder '{}'",
                dep,
                builder.name()
            ))
        })?;
        match &dep_node.state {
            PlanningState::Reused { realized, .. } => {
                realized_inputs.insert(*dep, realized.clone());
            }
            PlanningState::Unknown | PlanningState::NeedsBuild => return Ok(None),
        }
    }
    Ok(Some(realized_inputs))
}

fn completed_inputs_for_builder(
    completed: &HashMap<GraphKey, RealizedObject>,
    builder: &BuilderPlannedSubject,
) -> Result<HashMap<GraphKey, RealizedObject>, RuntimeError> {
    let mut realized_inputs = HashMap::new();
    for dep in builder.inputs().values() {
        let realized = completed.get(dep).cloned().ok_or_else(|| {
            RuntimeError::Build(format!(
                "dependency object '{}' is not available in completed set",
                dep
            ))
        })?;
        realized_inputs.insert(*dep, realized);
    }
    Ok(realized_inputs)
}

fn publish_reused_root(
    store: &Store,
    logger: &Arc<BuildRunLogger>,
    key: GraphKey,
    root_tag: &str,
    root_name: &str,
    realized: &RealizedObject,
    origin: ReuseOrigin,
) -> Result<(), RuntimeError> {
    let workspace = create_workspace(
        store,
        root_tag,
        Some(root_name.to_string()),
        key.to_string(),
    )
    .map(core_workspace)
    .map_err(map_store_error)?;
    let root_run = BuilderRun::new(
        root_tag.to_string(),
        Some(root_name.to_string()),
        key.to_string(),
        workspace,
    );
    let node_logger = logger
        .bind_builder(&root_run)
        .map_err(RuntimeError::Store)?;
    log_runtime_event(
        node_logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting builder node",
    );
    log_runtime_event(
        node_logger.as_ref(),
        BuildLogLevel::Info,
        match origin {
            ReuseOrigin::BuildHandle => "cache-hit",
            ReuseOrigin::CanonicalObject => "object-hit",
        },
        match origin {
            ReuseOrigin::BuildHandle => "reusing existing build ref",
            ReuseOrigin::CanonicalObject => "reusing existing canonical object",
        },
    );
    publish_stored_object(store, root_name, realized.object_hash).map_err(map_store_error)?;
    log_runtime_event(
        node_logger.as_ref(),
        BuildLogLevel::Info,
        "publish",
        format!("published '{}' -> {}", root_name, realized.object_hash),
    );
    node_logger.log_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        phase: "done".to_string(),
        message: "builder node completed".to_string(),
        object_hash: Some(realized.object_hash),
        raw_log_path: None,
        details: serde_json::Map::new(),
    });
    cleanup_workspace_temp_dir(store, root_run.temp_dir(), node_logger.as_ref());
    Ok(())
}

fn execute_misses(
    store: &Store,
    logger: Arc<BuildRunLogger>,
    nodes: &HashMap<GraphKey, GraphNode>,
    completed: &mut HashMap<GraphKey, RealizedObject>,
    root_key: GraphKey,
    jobs: usize,
    cancellation: CancellationToken,
) -> Result<RealizedObject, RuntimeError> {
    let mut remaining = HashMap::<GraphKey, usize>::new();
    let mut reverse = HashMap::<GraphKey, Vec<GraphKey>>::new();
    let mut ready = VecDeque::<GraphKey>::new();
    let mut first_error: Option<RuntimeError> = None;

    for (key, node) in nodes {
        if !matches!(node.state, PlanningState::NeedsBuild) {
            continue;
        }
        let mut wait_for = 0usize;
        if let Some(builder) = node.subject.as_builder() {
            for dep in builder.inputs().values() {
                if let Some(dep_node) = nodes.get(dep)
                    && matches!(dep_node.state, PlanningState::NeedsBuild)
                {
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

    let (tx, rx) = mpsc::channel::<(GraphKey, Result<SubjectExecution, RuntimeError>)>();
    let mut in_flight = HashMap::<GraphKey, JoinHandle<()>>::new();
    let mut last_wait_log: Option<Instant> = None;
    let scheduler_workspace = create_workspace(
        store,
        "Scheduler",
        Some("executor".to_string()),
        root_key.to_string(),
    )
    .map(core_workspace)
    .map_err(map_store_error)?;
    let scheduler_run = BuilderRun::new(
        "Scheduler",
        Some("executor".to_string()),
        root_key.to_string(),
        scheduler_workspace,
    );
    let scheduler_logger = logger
        .bind_builder(&scheduler_run)
        .map_err(RuntimeError::Store)?;
    cleanup_workspace_temp_dir(store, scheduler_run.temp_dir(), scheduler_logger.as_ref());

    while !completed.contains_key(&root_key) {
        if first_error.is_none() && cancellation.is_cancelled() {
            first_error = Some(RuntimeError::Cancelled(
                "build cancelled by signal".to_string(),
            ));
        }

        while first_error.is_none() && !cancellation.is_cancelled() && in_flight.len() < jobs {
            let Some(key) = ready.pop_front() else {
                break;
            };
            if completed.contains_key(&key) || in_flight.contains_key(&key) {
                continue;
            }
            let node = nodes.get(&key).ok_or_else(|| {
                RuntimeError::Store(format!("missing planned node for key '{}'", key))
            })?;
            let store = store.clone();
            let logger = logger.clone();
            let tx = tx.clone();
            let cancellation = cancellation.clone();
            let subject = node.subject.clone();
            let realized_inputs = match subject.as_builder() {
                Some(builder) => completed_inputs_for_builder(completed, builder)?,
                None => HashMap::new(),
            };
            let handle = thread::spawn(move || {
                let result = subject.execute(PlannedExecutionContext {
                    store: &store,
                    run_logger: logger,
                    cancellation,
                    realized_inputs: &realized_inputs,
                });
                let _ = tx.send((key, result));
            });
            in_flight.insert(key, handle);
        }

        if completed.contains_key(&root_key) {
            break;
        }

        if in_flight.is_empty() {
            if let Some(error) = first_error.take() {
                return Err(error);
            }
            return Err(RuntimeError::Build(
                "planner/executor stalled: no ready jobs and root object is still unresolved"
                    .to_string(),
            ));
        }

        let (key, result) = loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(message) => break message,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if first_error.is_none() && cancellation.is_cancelled() {
                        first_error = Some(RuntimeError::Cancelled(
                            "build cancelled by signal".to_string(),
                        ));
                    }
                    if let Some(error) = &first_error
                        && should_log_scheduler_wait(last_wait_log)
                    {
                        let details =
                            scheduler_wait_details(nodes, completed, &ready, &in_flight, error);
                        log_scheduler_event(
                            scheduler_logger.as_ref(),
                            BuildLogLevel::Warn,
                            "scheduler-wait",
                            "first error recorded; waiting for in-flight jobs to finish",
                            details,
                        );
                        last_wait_log = Some(Instant::now());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RuntimeError::Build(
                        "worker channel closed unexpectedly".to_string(),
                    ));
                }
            }
        };
        if let Some(handle) = in_flight.remove(&key) {
            handle.join().map_err(|_| {
                RuntimeError::Build(format!("worker thread for key '{}' panicked", key))
            })?;
        }
        let executed = match result {
            Ok(executed) => executed,
            Err(error) => {
                if first_error.is_none() {
                    let details = scheduler_first_error_details(
                        nodes, completed, &ready, &in_flight, key, &error,
                    );
                    log_scheduler_event(
                        scheduler_logger.as_ref(),
                        BuildLogLevel::Error,
                        "scheduler-error",
                        "worker returned first error; running jobs remain in flight",
                        details,
                    );
                    first_error = Some(error);
                    last_wait_log = None;
                }
                continue;
            }
        };
        let node = nodes.get(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for key '{}'", key))
        })?;
        let publication_name = node.subject.name();
        publish_stored_object(store, publication_name, executed.realized.object_hash)
            .map_err(map_store_error)?;
        log_runtime_event(
            executed.logger.as_ref(),
            BuildLogLevel::Info,
            "publish",
            format!(
                "published '{}' -> {}",
                publication_name, executed.realized.object_hash
            ),
        );
        executed.logger.log_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            phase: "done".to_string(),
            message: "builder node completed".to_string(),
            object_hash: Some(executed.realized.object_hash),
            raw_log_path: None,
            details: serde_json::Map::new(),
        });
        completed.insert(key, executed.realized);
        if first_error.is_none()
            && let Some(parents) = reverse.get(&key)
        {
            for parent in parents {
                let pending = remaining.get_mut(parent).ok_or_else(|| {
                    RuntimeError::Store(format!(
                        "missing pending-dependency counter for key '{}'",
                        parent
                    ))
                })?;
                *pending -= 1;
                if *pending == 0 {
                    ready.push_back(*parent);
                }
            }
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }
    completed.get(&root_key).cloned().ok_or_else(|| {
        RuntimeError::Store(format!(
            "root object for key '{}' is missing after executor completion",
            root_key
        ))
    })
}

fn should_log_scheduler_wait(last_wait_log: Option<Instant>) -> bool {
    last_wait_log
        .map(|instant| instant.elapsed() >= Duration::from_secs(5))
        .unwrap_or(true)
}

fn log_scheduler_event(
    logger: &dyn BuildLogger,
    level: BuildLogLevel,
    phase: &str,
    message: impl Into<String>,
    details: Map<String, Value>,
) {
    logger.log_event(BuildLogEvent {
        level,
        phase: phase.to_string(),
        message: message.into(),
        object_hash: None,
        raw_log_path: None,
        details,
    });
}

fn scheduler_first_error_details(
    nodes: &HashMap<GraphKey, GraphNode>,
    completed: &HashMap<GraphKey, RealizedObject>,
    ready: &VecDeque<GraphKey>,
    in_flight: &HashMap<GraphKey, JoinHandle<()>>,
    failed_key: GraphKey,
    error: &RuntimeError,
) -> Map<String, Value> {
    let mut details = scheduler_state_details(nodes, completed, ready, in_flight);
    details.insert("failed".to_string(), node_summary_value(nodes, failed_key));
    details.insert(
        "error_class".to_string(),
        Value::String(error.class().to_string()),
    );
    details.insert(
        "error_message".to_string(),
        Value::String(error.message().to_string()),
    );
    details
}

fn scheduler_wait_details(
    nodes: &HashMap<GraphKey, GraphNode>,
    completed: &HashMap<GraphKey, RealizedObject>,
    ready: &VecDeque<GraphKey>,
    in_flight: &HashMap<GraphKey, JoinHandle<()>>,
    error: &RuntimeError,
) -> Map<String, Value> {
    let mut details = scheduler_state_details(nodes, completed, ready, in_flight);
    details.insert(
        "first_error_class".to_string(),
        Value::String(error.class().to_string()),
    );
    details.insert(
        "first_error_message".to_string(),
        Value::String(error.message().to_string()),
    );
    details.insert(
        "wait_reason".to_string(),
        Value::String("in-flight jobs are allowed to finish after the first error".to_string()),
    );
    details
}

fn scheduler_state_details(
    nodes: &HashMap<GraphKey, GraphNode>,
    completed: &HashMap<GraphKey, RealizedObject>,
    ready: &VecDeque<GraphKey>,
    in_flight: &HashMap<GraphKey, JoinHandle<()>>,
) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert(
        "completed_count".to_string(),
        Value::Number((completed.len() as u64).into()),
    );
    details.insert(
        "ready_count".to_string(),
        Value::Number((ready.len() as u64).into()),
    );
    details.insert(
        "in_flight_count".to_string(),
        Value::Number((in_flight.len() as u64).into()),
    );
    details.insert(
        "in_flight".to_string(),
        Value::Array(in_flight_summaries(nodes, in_flight)),
    );
    details
}

fn in_flight_summaries(
    nodes: &HashMap<GraphKey, GraphNode>,
    in_flight: &HashMap<GraphKey, JoinHandle<()>>,
) -> Vec<Value> {
    let mut entries = in_flight
        .keys()
        .map(|key| (key.to_string(), node_summary_value(nodes, *key)))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries.into_iter().map(|(_, value)| value).collect()
}

fn node_summary_value(nodes: &HashMap<GraphKey, GraphNode>, key: GraphKey) -> Value {
    let mut object = Map::new();
    match key {
        GraphKey::ObjectKey(object_hash) => {
            object.insert("key_kind".to_string(), Value::String("object".to_string()));
            object.insert(
                "object_hash".to_string(),
                Value::String(object_hash.to_string()),
            );
        }
        GraphKey::BuildKey(build_key) => {
            object.insert("key_kind".to_string(), Value::String("build".to_string()));
            object.insert(
                "build_key".to_string(),
                Value::String(build_key.to_string()),
            );
        }
    }
    object.insert("short_key".to_string(), Value::String(key.short()));
    if let Some(node) = nodes.get(&key) {
        object.insert(
            "tag".to_string(),
            Value::String(node.subject.tag().to_string()),
        );
        object.insert(
            "name".to_string(),
            Value::String(node.subject.name().to_string()),
        );
    }
    Value::Object(object)
}

fn build_run_logger_for_store(
    store: &Store,
    emit_progress: bool,
) -> Result<BuildRunLogger, String> {
    let locations = store.run_log_locations();
    BuildRunLogger::new(locations.run_log_dir(), locations.run_id(), emit_progress)
}

fn core_workspace(workspace: StoreWorkspace) -> Workspace {
    Workspace::new(
        workspace.log_dir().to_path_buf(),
        workspace.raw_log_dir().to_path_buf(),
        workspace.temp_dir().to_path_buf(),
    )
}

fn cleanup_workspace_temp_dir(store: &Store, temp_dir: &Path, logger: &dyn BuildLogger) {
    if let Err(error) = remove_store_temp_dir_force(store, temp_dir) {
        log_runtime_event(
            logger,
            BuildLogLevel::Warn,
            "cleanup-warning",
            format!(
                "failed to remove temp dir '{}': {error}",
                temp_dir.display()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planned::{PlannedSubject, ReuseOrigin, SourcePlannedSubject};
    use crate::recipe::collect_graph;
    use bobr_store::identity::compute_reuse_key;
    use bobr_store::{PublishRequest, ReuseInputIdentity, publish_build};
    use mbuild_core::{CancellationToken, OriginContext, OriginSpec, ParsedOrigin};
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn create_test_store(root: &Path) -> Store {
        let store_root = root.join(".mbuild");
        fs::create_dir_all(&store_root).unwrap();
        Store::create(&store_root).unwrap()
    }

    fn create_test_logger(store: &Store) -> Arc<BuildRunLogger> {
        Arc::new(build_run_logger_for_store(store, false).unwrap())
    }

    fn workspace_metadata(root: &Path, tag: &str, name: &str) -> serde_json::Value {
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
                let metadata: serde_json::Value =
                    serde_json::from_slice(&fs::read(meta_path).unwrap()).unwrap();
                if metadata["tag"] == tag && metadata["recipe_name"] == name {
                    return metadata;
                }
            }
        }
        panic!("missing workspace metadata for {tag}/{name}");
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

    fn sample_realized(build_key: Option<BuildKey>, object_hash: &str) -> RealizedObject {
        let object_hash = object_hash.parse().unwrap();
        RealizedObject {
            build_key,
            object_hash,
            run_id: None,
        }
    }

    fn expect_build_key(key: GraphKey) -> BuildKey {
        match key {
            GraphKey::BuildKey(build_key) => build_key,
            GraphKey::ObjectKey(object_hash) => {
                panic!("expected build graph key, got object key {object_hash}")
            }
        }
    }

    #[test]
    fn planned_subject_canonical_lookup_uses_dependency_object_hashes() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let request = RecipeEnvelope::parse_json(
            br##"{
                "options": {
                "store": "/tmp/unused-store"
                },
                "nodes": {
                    "root": {
                        "name": "sandbox",
                        "tag": "Sandbox",
                        "config": {},
                        "inputs": {
                            "rootfs": "rootfs",
                            "script": "script"
                        }
                    },
                    "rootfs": {
                        "name": "rootfs",
                        "tag": "Tree",
                        "config": {
                            "tree": {
                                "entries": [{
                                    "type": "file",
                                    "path": "rootfs.txt",
                                    "text": "rootfs",
                                    "executable": false
                                }]
                            }
                        },
                        "inputs": {}
                    },
                    "script": {
                        "name": "script",
                        "tag": "Tree",
                        "config": {
                            "tree": {
                                "entries": [{
                                    "type": "file",
                                    "path": "script.sh",
                                    "text": "#!/bin/sh\nexit 0\n",
                                    "executable": true
                                }]
                            }
                        },
                        "inputs": {}
                    }
                }
            }"##,
        )
        .unwrap()
        .request;

        let mut nodes = HashMap::new();
        let root_key = collect_graph(&request, &mut nodes).unwrap();
        let root_build_key = expect_build_key(root_key);
        let dep_keys = {
            let subject = nodes.get(&root_key).unwrap().subject.as_ref();
            match subject {
                PlannedSubject::Builder(builder) => {
                    builder.inputs().values().copied().collect::<Vec<_>>()
                }
                PlannedSubject::Source(_) => panic!("expected builder root subject"),
            }
        };
        assert_eq!(dep_keys.len(), 2);

        let rootfs_realized = sample_realized(
            Some(expect_build_key(dep_keys[0])),
            "1111111111111111111111111111111111111111111111111111111111111111",
        );
        let script_realized = sample_realized(
            Some(expect_build_key(dep_keys[1])),
            "2222222222222222222222222222222222222222222222222222222222222222",
        );
        nodes.get_mut(&dep_keys[0]).unwrap().state = PlanningState::Reused {
            realized: rootfs_realized.clone(),
            origin: ReuseOrigin::CanonicalObject,
        };
        nodes.get_mut(&dep_keys[1]).unwrap().state = PlanningState::Reused {
            realized: script_realized.clone(),
            origin: ReuseOrigin::CanonicalObject,
        };

        let root_inputs = vec![
            ReuseInputIdentity {
                object_hash: rootfs_realized.object_hash,
            },
            ReuseInputIdentity {
                object_hash: script_realized.object_hash,
            },
        ];
        let reuse_key = compute_reuse_key("Sandbox", &json!({}), &root_inputs).unwrap();
        let stage_dir = temp.path().join("root-out");
        fs::create_dir_all(&stage_dir).unwrap();
        fs::write(stage_dir.join("payload"), b"ok\n").unwrap();
        publish_build(
            &store,
            PublishRequest {
                publication_name: "bin".to_string(),
                build_key: root_build_key,
                reuse_key,
                staged_path: stage_dir,
                inputs: root_inputs,
            },
        )
        .unwrap();

        let realized_inputs = HashMap::from([
            (dep_keys[0], rootfs_realized),
            (dep_keys[1], script_realized),
        ]);
        let published = nodes
            .get(&root_key)
            .unwrap()
            .subject
            .lookup_after_inputs_reused(PlannedDependencyLookupContext {
                store: &store,
                realized_inputs: &realized_inputs,
            })
            .unwrap()
            .expect("expected canonical object hit");
        assert_eq!(published.realized.build_key, Some(root_build_key));
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
        let subject = SourcePlannedSubject::new(
            "cancel-source".to_string(),
            object_hash,
            Some(Box::new(CancellingOrigin {
                cancellation: cancellation.clone(),
            })),
        );

        let realized_inputs = HashMap::new();
        let error = subject
            .execute(PlannedExecutionContext {
                store: &store,
                run_logger: logger,
                cancellation,
                realized_inputs: &realized_inputs,
            })
            .expect_err("expected cancellation");

        assert_eq!(error.class(), "cancelled");
        let metadata = workspace_metadata(temp.path(), "Source", "cancel-source");
        let temp_dir = PathBuf::from(metadata["temp_dir"].as_str().unwrap());
        assert!(!temp_dir.exists());
    }
}
