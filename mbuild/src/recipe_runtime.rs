use crate::planned::{
    BuilderPlannedSubject, PlannedExecutionContext, PlannedSubject, ReuseOrigin, SubjectExecution,
    execute_subject, realized_object_from_record,
};
use crate::recipe::{RecipeEnvelope, collect_graph};
use crate::runtime::{RuntimeError, check_cancelled, log_runtime_event, map_store_error};
use bobr_store::{
    RealizedObject, Store, StoreWorkspace, create_workspace, load_build_handle,
    publish_stored_object, remove_store_temp_dir_force, resolve_reuse_for_build,
};
use mbuild_core::{
    BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, BuilderRun,
    CancellationToken, Workspace,
};
use serde_json::to_string_pretty;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;

type SubjectGraph = HashMap<BuildKey, Arc<PlannedSubject>>;
type PlanningStates = HashMap<BuildKey, PlanningState>;

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
    let mut subjects = HashMap::new();
    let root_key = collect_graph(&request, &mut subjects)?;
    let root_subject = subjects.get(&root_key).ok_or_else(|| {
        RuntimeError::Store(format!("missing root subject for key '{}'", root_key))
    })?;
    let root_name = root_subject.name().to_string();
    let root_tag = root_subject.tag().to_string();

    let store = Store::create(store_path).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> =
        Arc::new(build_run_logger_for_store(&store, emit_progress).map_err(RuntimeError::Store)?);

    let mut states = HashMap::new();
    resolve_planning_state(&store, &subjects, &mut states, root_key)?;

    let mut completed = HashMap::new();
    for (key, state) in &states {
        if let PlanningState::Reused { realized, .. } = state {
            completed.insert(*key, realized.clone());
        }
    }

    if let Some(realized) = completed.get(&root_key).cloned() {
        let origin = match states.get(&root_key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planning state for key '{}'", root_key))
        })? {
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

    execute_misses(ExecuteMissesContext {
        store: &store,
        logger,
        subjects: &subjects,
        states: &states,
        completed: &mut completed,
        root_key,
        jobs,
        cancellation,
    })
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

#[derive(Debug, Clone)]
enum PlanningState {
    Reused {
        realized: RealizedObject,
        origin: ReuseOrigin,
    },
    NeedsBuild,
}

fn resolve_planning_state(
    store: &Store,
    subjects: &SubjectGraph,
    states: &mut PlanningStates,
    key: BuildKey,
) -> Result<(), RuntimeError> {
    let subject = {
        let subject = subjects.get(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned subject for key '{}'", key))
        })?;
        if states.contains_key(&key) {
            return Ok(());
        }
        subject.clone()
    };

    if let Some(published) =
        load_build_handle(store, subject.build_key()).map_err(map_store_error)?
    {
        states.insert(
            key,
            PlanningState::Reused {
                realized: realized_object_from_record(
                    Some(published.build.build_key),
                    &published.object_record,
                ),
                origin: ReuseOrigin::BuildHandle,
            },
        );
        return Ok(());
    }

    let Some(builder) = subject.as_builder() else {
        states.insert(key, PlanningState::NeedsBuild);
        return Ok(());
    };

    for dep in builder.inputs().values().copied().collect::<Vec<_>>() {
        resolve_planning_state(store, subjects, states, dep)?;
    }

    let Some(realized_inputs) = reused_inputs_for_builder(states, builder)? else {
        states.insert(key, PlanningState::NeedsBuild);
        return Ok(());
    };

    let reuse_key = builder.reuse_key_for_realized_inputs(&realized_inputs)?;
    if let Some(published) =
        resolve_reuse_for_build(store, builder.build_key(), reuse_key).map_err(map_store_error)?
    {
        states.insert(
            key,
            PlanningState::Reused {
                realized: realized_object_from_record(
                    Some(builder.build_key()),
                    &published.object_record,
                ),
                origin: ReuseOrigin::CanonicalObject,
            },
        );
    } else {
        states.insert(key, PlanningState::NeedsBuild);
    }

    Ok(())
}

fn reused_inputs_for_builder(
    states: &PlanningStates,
    builder: &BuilderPlannedSubject,
) -> Result<Option<HashMap<BuildKey, RealizedObject>>, RuntimeError> {
    let mut realized_inputs = HashMap::new();
    for dep in builder.inputs().values() {
        let dep_state = states.get(dep).ok_or_else(|| {
            RuntimeError::Store(format!(
                "missing dependency state '{}' for builder '{}'",
                dep,
                builder.name()
            ))
        })?;
        match dep_state {
            PlanningState::Reused { realized, .. } => {
                realized_inputs.insert(*dep, realized.clone());
            }
            PlanningState::NeedsBuild => return Ok(None),
        }
    }
    Ok(Some(realized_inputs))
}

fn completed_inputs_for_builder(
    completed: &HashMap<BuildKey, RealizedObject>,
    builder: &BuilderPlannedSubject,
) -> Result<HashMap<BuildKey, RealizedObject>, RuntimeError> {
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
    key: BuildKey,
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

struct ExecuteMissesContext<'a> {
    store: &'a Store,
    logger: Arc<BuildRunLogger>,
    subjects: &'a SubjectGraph,
    states: &'a PlanningStates,
    completed: &'a mut HashMap<BuildKey, RealizedObject>,
    root_key: BuildKey,
    jobs: usize,
    cancellation: CancellationToken,
}

fn execute_misses(cx: ExecuteMissesContext<'_>) -> Result<RealizedObject, RuntimeError> {
    let ExecuteMissesContext {
        store,
        logger,
        subjects,
        states,
        completed,
        root_key,
        jobs,
        cancellation,
    } = cx;

    let mut remaining = HashMap::<BuildKey, usize>::new();
    let mut reverse = HashMap::<BuildKey, Vec<BuildKey>>::new();
    let mut ready = VecDeque::<BuildKey>::new();
    let mut first_error: Option<RuntimeError> = None;

    for (key, state) in states {
        if !matches!(state, PlanningState::NeedsBuild) {
            continue;
        }
        let subject = subjects.get(key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned subject for key '{key}'"))
        })?;
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

    let (tx, rx) = mpsc::channel::<(BuildKey, Result<SubjectExecution, RuntimeError>)>();
    let mut in_flight = 0usize;

    while !completed.contains_key(&root_key) {
        if first_error.is_none() && cancellation.is_cancelled() {
            first_error = Some(RuntimeError::Cancelled(
                "build cancelled by signal".to_string(),
            ));
        }

        while first_error.is_none() && !cancellation.is_cancelled() && in_flight < jobs {
            let Some(key) = ready.pop_front() else {
                break;
            };
            let subject = subjects.get(&key).ok_or_else(|| {
                RuntimeError::Store(format!("missing planned subject for key '{}'", key))
            })?;
            let store = store.clone();
            let logger = logger.clone();
            let tx = tx.clone();
            let cancellation = cancellation.clone();
            let subject = subject.clone();
            let realized_inputs = match subject.as_builder() {
                Some(builder) => completed_inputs_for_builder(completed, builder)?,
                None => HashMap::new(),
            };
            thread::spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    execute_subject(
                        &subject,
                        PlannedExecutionContext {
                            store: &store,
                            run_logger: logger,
                            cancellation,
                            realized_inputs: &realized_inputs,
                        },
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(RuntimeError::Build(format!(
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
            if let Some(error) = first_error.take() {
                return Err(error);
            }
            return Err(RuntimeError::Build(
                "planner/executor stalled: no ready jobs and root object is still unresolved"
                    .to_string(),
            ));
        }

        let (key, result) = rx
            .recv()
            .map_err(|_| RuntimeError::Build("worker channel closed unexpectedly".to_string()))?;
        in_flight -= 1;
        let executed = match result {
            Ok(executed) => executed,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
        };
        let subject = subjects.get(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned subject for key '{}'", key))
        })?;
        let publication_name = subject.name();
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
    use crate::planned::PlannedSubject;
    use crate::recipe::collect_graph;
    use bobr_store::{PublishRequest, publish_build};
    use mbuild_core::{
        CancellationToken, OriginContext, OriginSpec, ParsedOrigin, compute_reuse_key,
    };
    use mbuild_source::SourcePlannedSubject;
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

        let mut subjects = HashMap::new();
        let root_key = collect_graph(&request, &mut subjects).unwrap();
        let root_build_key = root_key;
        let dep_keys = {
            let subject = subjects.get(&root_key).unwrap().as_ref();
            match subject {
                PlannedSubject::Builder(builder) => {
                    builder.inputs().values().copied().collect::<Vec<_>>()
                }
                PlannedSubject::Source(_) => panic!("expected builder root subject"),
            }
        };
        assert_eq!(dep_keys.len(), 2);

        let rootfs_realized = sample_realized(
            Some(dep_keys[0]),
            "1111111111111111111111111111111111111111111111111111111111111111",
        );
        let script_realized = sample_realized(
            Some(dep_keys[1]),
            "2222222222222222222222222222222222222222222222222222222222222222",
        );
        let root_inputs = vec![rootfs_realized.object_hash, script_realized.object_hash];
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
        let root_builder = subjects
            .get(&root_key)
            .unwrap()
            .as_ref()
            .as_builder()
            .expect("expected builder root subject");
        let actual_reuse_key = root_builder
            .reuse_key_for_realized_inputs(&realized_inputs)
            .unwrap();
        assert_eq!(actual_reuse_key, reuse_key);
        let published = resolve_reuse_for_build(&store, root_build_key, actual_reuse_key)
            .unwrap()
            .expect("expected canonical object hit");
        assert_eq!(published.build.build_key, root_build_key);
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
}
