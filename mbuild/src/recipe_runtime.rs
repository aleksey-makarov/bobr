use crate::builder_registry::create_builder_registry;
use crate::planned::{
    BuilderPlannedSubject, PlannedExecutionContext, PlannedSubject, SubjectExecution,
    execute_subject,
};
use crate::recipe::{RecipeEnvelope, collect_graph};
use crate::runtime::{RuntimeError, check_cancelled, log_runtime_event, map_store_error};
use bobr_store::{RealizedObject, Store, publish_stored_object};
use mbuild_core::{BuildKey, BuildLogEvent, BuildLogLevel, BuildRunLogger, CancellationToken};
use serde_json::to_string_pretty;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;

type SubjectGraph = HashMap<BuildKey, Arc<PlannedSubject>>;

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
    let builder_registry = create_builder_registry()?;
    let mut subjects = HashMap::new();
    let root_key = collect_graph(&request, &builder_registry, &mut subjects)?;

    let store = Store::create(store_path).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> =
        Arc::new(build_run_logger_for_store(&store, emit_progress).map_err(RuntimeError::Store)?);

    execute_graph(&store, logger, &subjects, root_key, jobs, cancellation)
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

fn execute_graph(
    store: &Store,
    logger: Arc<BuildRunLogger>,
    subjects: &SubjectGraph,
    root_key: BuildKey,
    jobs: usize,
    cancellation: CancellationToken,
) -> Result<RealizedObject, RuntimeError> {
    let mut completed = HashMap::<BuildKey, RealizedObject>::new();
    let mut remaining = HashMap::<BuildKey, usize>::new();
    let mut reverse = HashMap::<BuildKey, Vec<BuildKey>>::new();
    let mut ready = VecDeque::<BuildKey>::new();
    let mut first_error: Option<RuntimeError> = None;

    for (key, subject) in subjects {
        let mut wait_for = 0usize;
        if let Some(builder) = subject.as_builder() {
            for dep in builder.inputs().values() {
                wait_for += 1;
                reverse.entry(*dep).or_default().push(*key);
            }
        }
        remaining.insert(*key, wait_for);
        if wait_for == 0 {
            ready.push_back(*key);
        }
    }

    let (tx, rx) = mpsc::channel::<(BuildKey, Result<SubjectExecution, RuntimeError>)>();
    let mut in_flight = 0usize;

    // Workers run on detached threads tracked only by `in_flight`. Once a
    // worker is spawned, scheduler-side failures should be recorded in
    // `first_error` so the loop stops launching new work and drains already
    // running workers before returning. The channel-disconnected case below is
    // different: it means no sender remains, so no worker can report a result.
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
            let subject = match subjects.get(&key) {
                Some(subject) => subject.clone(),
                None => {
                    first_error = Some(RuntimeError::Store(format!(
                        "missing planned subject for key '{}'",
                        key
                    )));
                    break;
                }
            };
            let store = store.clone();
            let logger = logger.clone();
            let tx = tx.clone();
            let cancellation = cancellation.clone();
            let realized_inputs = match subject.as_builder() {
                Some(builder) => match completed_inputs_for_builder(&completed, builder) {
                    Ok(realized_inputs) => realized_inputs,
                    Err(error) => {
                        first_error = Some(error);
                        break;
                    }
                },
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

        // Unreachable while this function holds its own `tx`: the channel can
        // only disconnect once every sender is dropped, and disconnect would
        // mean no worker can report a result anyway. The `?` is a defensive
        // return for that impossible case (not an abandon: nothing to drain).
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
        let subject = match subjects.get(&key) {
            Some(subject) => subject,
            None => {
                first_error.get_or_insert(RuntimeError::Store(format!(
                    "missing planned subject for key '{}'",
                    key
                )));
                continue;
            }
        };
        let publication_name = subject.name();
        if let Err(error) =
            publish_stored_object(store, publication_name, executed.realized.object_hash)
                .map_err(map_store_error)
        {
            first_error.get_or_insert(error);
            continue;
        }
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
            message: "subject completed".to_string(),
            object_hash: Some(executed.realized.object_hash),
            raw_log_path: None,
            details: serde_json::Map::new(),
        });
        completed.insert(key, executed.realized);
        if first_error.is_none()
            && let Some(parents) = reverse.get(&key)
        {
            for parent in parents {
                let Some(pending) = remaining.get_mut(parent) else {
                    first_error.get_or_insert(RuntimeError::Store(format!(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planned::PlannedSubject;
    use mbuild_core::{CancellationToken, OriginContext, OriginSpec, ParsedOrigin};
    use mbuild_source::SourcePlannedSubject;
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
        use crate::planned::{BuilderPlannedSubject, PlannedSubject};
        use mbuild_core::{
            BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder,
        };
        use serde::Deserialize;
        use serde_json::json;
        use std::collections::BTreeMap;
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
                object_hash: None,
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
            ) -> Result<StagedBuildResult, mbuild_core::BuilderError> {
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
            ) -> Result<StagedBuildResult, mbuild_core::BuilderError> {
                std::thread::sleep(Duration::from_millis(300));
                SLOW_FINISHED.store(true, Ordering::SeqCst);
                Ok(stage_payload(cx, b"slow\n"))
            }
        }

        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);

        // Fast node has an invalid publication name, so its publish fails after
        // the build succeeds. Slow node is a sibling that is still in flight.
        let fast = BuilderPlannedSubject::new(
            &FAST_BUILDER,
            "bad/name".to_string(),
            json!({}),
            BTreeMap::new(),
        )
        .unwrap();
        let slow = BuilderPlannedSubject::new(
            &SLOW_BUILDER,
            "good".to_string(),
            json!({}),
            BTreeMap::new(),
        )
        .unwrap();
        let fast_key = fast.build_key();
        let slow_key = slow.build_key();

        let mut subjects: SubjectGraph = HashMap::new();
        subjects.insert(fast_key, Arc::new(PlannedSubject::Builder(fast)));
        subjects.insert(slow_key, Arc::new(PlannedSubject::Builder(slow)));

        // Root is the failing node, so the loop never completes through the root
        // and must drain the in-flight slow worker before returning the error.
        let error = execute_graph(
            &store,
            logger,
            &subjects,
            fast_key,
            2,
            CancellationToken::new(),
        )
        .expect_err("expected publish failure for invalid publication name");

        assert_eq!(error.class(), "store");
        assert!(
            SLOW_FINISHED.load(Ordering::SeqCst),
            "execute_graph returned before draining the in-flight worker"
        );
    }
}
