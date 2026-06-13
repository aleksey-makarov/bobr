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
    let mut subjects = HashMap::new();
    let root_key = collect_graph(&request, &mut subjects)?;

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
                Some(builder) => completed_inputs_for_builder(&completed, builder)?,
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
