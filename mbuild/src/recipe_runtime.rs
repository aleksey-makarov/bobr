use crate::builders;
use crate::recipe::{
    PlannedBuilderRecipe, PlannedNode, PlannedRecipe, PlannedSourceRecipe, PlanningState,
    RecipeEnvelope, RecipePaths, RecipeRequest, ReuseOrigin, collect_graph,
};
use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use crate::runtime::{
    ExecuteBuilderNodeRequest, RuntimeError, check_cancelled, execute_builder_node,
    log_runtime_event, lookup_build_handle, lookup_canonical_result, map_store_error,
};
use mbuild_core::{
    BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, CancellationToken, OriginContext,
    RunOptions,
};
use mbuild_store::{
    BuildKey, RealizedResult, ResultRecord, ReuseInputIdentity, SourceImportOutcome, SourceLookup,
    Store, import_source_result, lookup_source_result, publish_result,
    recreate_store_temp_dir_force, remove_store_temp_dir_force,
};
use serde_json::{Map, Value, to_string_pretty};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct BuildRunOptions {
    pub emit_progress: bool,
    pub jobs: usize,
    pub cancellation: CancellationToken,
}

impl Default for BuildRunOptions {
    fn default() -> Self {
        Self {
            emit_progress: false,
            jobs: 1,
            cancellation: CancellationToken::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct ExecutedNode {
    realized: RealizedResult,
}

pub fn run_recipe_json_in_workspace(
    workspace_root: &Path,
    recipe_path: &Path,
) -> Result<RealizedResult, RuntimeError> {
    run_recipe_json_in_workspace_with_options(
        workspace_root,
        recipe_path,
        BuildRunOptions::default(),
    )
}

pub fn run_recipe_json_in_workspace_with_options(
    _workspace_root: &Path,
    recipe_path: &Path,
    cli_options: BuildRunOptions,
) -> Result<RealizedResult, RuntimeError> {
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
    let options = BuildRunOptions {
        emit_progress: cli_options.emit_progress,
        jobs: if cli_options.jobs == BuildRunOptions::default().jobs {
            envelope.options.jobs.unwrap_or(cli_options.jobs)
        } else {
            cli_options.jobs
        },
        cancellation: cli_options.cancellation.clone(),
    };
    let options = BuildRunOptions {
        emit_progress: if cli_options.emit_progress == BuildRunOptions::default().emit_progress {
            !envelope.options.quiet.unwrap_or(!cli_options.emit_progress)
        } else {
            cli_options.emit_progress
        },
        jobs: options.jobs,
        cancellation: options.cancellation,
    };
    if options.jobs == 0 {
        return Err(RuntimeError::InvalidRequest(
            "--jobs and recipe options.jobs must be greater than zero".to_string(),
        ));
    }
    validate_runtime_paths(&envelope.paths)?;
    run_recipe_request_in_store_with_options(&envelope.paths, envelope.request, options)
}

pub fn run_recipe_request_in_store_with_options(
    paths: &RecipePaths,
    request: RecipeRequest,
    options: BuildRunOptions,
) -> Result<RealizedResult, RuntimeError> {
    if options.jobs == 0 {
        return Err(RuntimeError::InvalidRequest(
            "--jobs must be greater than zero".to_string(),
        ));
    }
    check_cancelled(&options.cancellation)?;
    validate_runtime_paths(paths)?;

    let layout = Store::create(&paths.store).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> = Arc::new(
        BuildRunLogger::new(
            layout.root(),
            RunOptions {
                emit_progress: options.emit_progress,
            },
        )
        .map_err(RuntimeError::Store)?,
    );

    let mut nodes = HashMap::new();
    let graph = collect_graph(&request, "root", &mut nodes)?;
    let root_key = graph.root_key;
    let root_recipe = request.node("root")?;
    let root_name = root_recipe.name().to_string();
    let root_tag = root_recipe.tag().to_string();
    ensure_planned(&layout, &mut nodes, root_key, logger.created_at())?;

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
            &layout, &logger, root_key, &root_tag, &root_name, &realized, origin,
        )?;
        return Ok(realized);
    }

    execute_misses(
        &layout,
        logger,
        &nodes,
        &mut completed,
        root_key,
        options.jobs,
        options.cancellation,
    )
}

pub fn render_result_as_json(result: &RealizedResult) -> Result<String, RuntimeError> {
    let mut rendered = to_string_pretty(result).map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to render realized result as JSON: {error}"))
    })?;
    rendered.push('\n');
    Ok(rendered)
}

fn validate_runtime_paths(paths: &RecipePaths) -> Result<(), RuntimeError> {
    validate_existing_dir(&paths.store, "store path")?;
    Ok(())
}

fn validate_existing_dir(path: &Path, label: &str) -> Result<(), RuntimeError> {
    let metadata = fs::metadata(path).map_err(|error| {
        RuntimeError::RecipeLoad(format!(
            "{label} '{}' does not exist or is not accessible: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{label} '{}' is not a directory",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_planned(
    layout: &Store,
    nodes: &mut HashMap<BuildKey, PlannedNode>,
    key: BuildKey,
    created_at: &str,
) -> Result<(), RuntimeError> {
    let recipe = {
        let node = nodes.get(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for key '{}'", key))
        })?;
        if !matches!(node.state, PlanningState::Unknown) {
            return Ok(());
        }
        node.recipe.clone()
    };

    match &recipe {
        PlannedRecipe::Builder(_) => {
            if let Some(published) = lookup_build_handle(layout, key)? {
                let node = nodes.get_mut(&key).ok_or_else(|| {
                    RuntimeError::Store(format!("missing planned node for key '{}'", key))
                })?;
                node.state = PlanningState::Reused {
                    realized: realized_result_from_record(
                        Some(published.build.build_key),
                        &published.result,
                    ),
                    origin: ReuseOrigin::BuildHandle,
                };
                return Ok(());
            }

            let mut deps = Vec::new();
            recipe.try_for_each_direct_dep(|dep| {
                deps.push(dep);
                Ok::<_, RuntimeError>(())
            })?;
            for dep in deps {
                ensure_planned(layout, nodes, dep, created_at)?;
            }

            if let Some(realized) = lookup_canonical_for_planned_node(layout, nodes, key)? {
                let node = nodes.get_mut(&key).ok_or_else(|| {
                    RuntimeError::Store(format!("missing planned node for key '{}'", key))
                })?;
                node.state = PlanningState::Reused {
                    realized,
                    origin: ReuseOrigin::CanonicalResult,
                };
            } else {
                let node = nodes.get_mut(&key).ok_or_else(|| {
                    RuntimeError::Store(format!("missing planned node for key '{}'", key))
                })?;
                node.state = PlanningState::NeedsBuild;
            }
        }
        PlannedRecipe::Source(source) => {
            let node = nodes.get_mut(&key).ok_or_else(|| {
                RuntimeError::Store(format!("missing planned node for key '{}'", key))
            })?;
            match lookup_source_result(layout, source.object_hash, created_at)
                .map_err(map_store_error)?
            {
                SourceLookup::Hit(stored) => {
                    node.state = PlanningState::Reused {
                        realized: realized_result_from_record(None, &stored.result),
                        origin: ReuseOrigin::CanonicalResult,
                    };
                }
                SourceLookup::Missing => {
                    node.state = PlanningState::NeedsBuild;
                }
            }
        }
    }

    Ok(())
}

fn publish_reused_root(
    layout: &Store,
    logger: &Arc<BuildRunLogger>,
    key: BuildKey,
    root_tag: &str,
    root_name: &str,
    realized: &RealizedResult,
    origin: ReuseOrigin,
) -> Result<(), RuntimeError> {
    let node_logger = logger.bind_node(root_tag, root_name, key);
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
            ReuseOrigin::CanonicalResult => "result-hit",
        },
        match origin {
            ReuseOrigin::BuildHandle => "reusing existing build ref",
            ReuseOrigin::CanonicalResult => "reusing existing canonical result",
        },
    );
    publish_result(layout, root_name, realized.result_id).map_err(map_store_error)?;
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
    Ok(())
}

fn lookup_canonical_for_planned_node(
    layout: &Store,
    nodes: &HashMap<BuildKey, PlannedNode>,
    key: BuildKey,
) -> Result<Option<RealizedResult>, RuntimeError> {
    let node = nodes
        .get(&key)
        .ok_or_else(|| RuntimeError::Store(format!("missing planned node for key '{}'", key)))?;
    let Some((spec, config, _inputs)) = node.recipe.builder() else {
        return Ok(None);
    };

    let mut input_identities = Vec::<ReuseInputIdentity>::new();
    let mut all_reused = true;
    node.recipe.try_for_each_direct_dep(|dep| {
        let dep_node = nodes.get(&dep).ok_or_else(|| {
            RuntimeError::Store(format!(
                "missing dependency node '{}' for key '{}'",
                dep, key
            ))
        })?;
        match &dep_node.state {
            PlanningState::Reused { realized, .. } => input_identities.push(ReuseInputIdentity {
                object_hash: realized.object_hash,
            }),
            PlanningState::Unknown | PlanningState::NeedsBuild => all_reused = false,
        }
        Ok(())
    })?;
    if !all_reused {
        return Ok(None);
    }

    Ok(
        lookup_canonical_result(layout, spec.tag, config, &input_identities, key)?
            .map(|published| realized_result_from_record(Some(key), &published.result)),
    )
}

fn execute_misses(
    layout: &Store,
    logger: Arc<BuildRunLogger>,
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &mut HashMap<BuildKey, RealizedResult>,
    root_key: BuildKey,
    jobs: usize,
    cancellation: CancellationToken,
) -> Result<RealizedResult, RuntimeError> {
    let mut remaining = HashMap::<BuildKey, usize>::new();
    let mut reverse = HashMap::<BuildKey, Vec<BuildKey>>::new();
    let mut ready = VecDeque::<BuildKey>::new();
    let mut first_error: Option<RuntimeError> = None;

    for (key, node) in nodes {
        if !matches!(node.state, PlanningState::NeedsBuild) {
            continue;
        }
        let mut wait_for = 0usize;
        node.recipe
            .try_for_each_direct_dep(|dep| -> Result<(), RuntimeError> {
                if let Some(dep_node) = nodes.get(&dep)
                    && matches!(dep_node.state, PlanningState::NeedsBuild)
                {
                    wait_for += 1;
                    reverse.entry(dep).or_default().push(*key);
                }
                Ok(())
            })?;
        remaining.insert(*key, wait_for);
        if wait_for == 0 {
            ready.push_back(*key);
        }
    }

    let (tx, rx) = mpsc::channel::<(BuildKey, Result<ExecutedNode, RuntimeError>)>();
    let mut in_flight = HashMap::<BuildKey, JoinHandle<()>>::new();
    let mut last_wait_log: Option<Instant> = None;

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
            let layout = layout.clone();
            let logger = logger.clone();
            let tx = tx.clone();
            let cancellation = cancellation.clone();
            let recipe = node.recipe.clone();
            let builder_inputs = match &recipe {
                PlannedRecipe::Builder(builder_recipe) => {
                    Some(build_resolved_inputs(&layout, builder_recipe, completed)?)
                }
                PlannedRecipe::Source(_) => None,
            };
            let handle = thread::spawn(move || {
                let result = match recipe {
                    PlannedRecipe::Builder(builder_recipe) => execute_builder_recipe(
                        &layout,
                        logger,
                        key,
                        builder_recipe,
                        cancellation,
                        builder_inputs.expect("builder inputs must be prepared"),
                    ),
                    PlannedRecipe::Source(source_recipe) => {
                        execute_source_recipe(&layout, logger, key, cancellation, source_recipe)
                    }
                };
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
                "planner/executor stalled: no ready jobs and root result is still unresolved"
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
                            &logger,
                            root_key,
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
                        &logger,
                        root_key,
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
        let node_logger = logger.bind_node(node.recipe.tag(), &node.publish_name, key);
        publish_result(layout, &node.publish_name, executed.realized.result_id)
            .map_err(map_store_error)?;
        log_runtime_event(
            node_logger.as_ref(),
            BuildLogLevel::Info,
            "publish",
            format!(
                "published '{}' -> {}",
                node.publish_name, executed.realized.object_hash
            ),
        );
        node_logger.log_event(BuildLogEvent {
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
            "root result for key '{}' is missing after executor completion",
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
    logger: &Arc<BuildRunLogger>,
    root_key: BuildKey,
    level: BuildLogLevel,
    phase: &str,
    message: impl Into<String>,
    details: Map<String, Value>,
) {
    logger
        .bind_node("Scheduler", "executor", root_key)
        .log_event(BuildLogEvent {
            level,
            phase: phase.to_string(),
            message: message.into(),
            object_hash: None,
            raw_log_path: None,
            details,
        });
}

fn scheduler_first_error_details(
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &HashMap<BuildKey, RealizedResult>,
    ready: &VecDeque<BuildKey>,
    in_flight: &HashMap<BuildKey, JoinHandle<()>>,
    failed_key: BuildKey,
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
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &HashMap<BuildKey, RealizedResult>,
    ready: &VecDeque<BuildKey>,
    in_flight: &HashMap<BuildKey, JoinHandle<()>>,
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
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &HashMap<BuildKey, RealizedResult>,
    ready: &VecDeque<BuildKey>,
    in_flight: &HashMap<BuildKey, JoinHandle<()>>,
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
    nodes: &HashMap<BuildKey, PlannedNode>,
    in_flight: &HashMap<BuildKey, JoinHandle<()>>,
) -> Vec<Value> {
    let mut entries = in_flight
        .keys()
        .map(|key| (key.to_string(), node_summary_value(nodes, *key)))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries.into_iter().map(|(_, value)| value).collect()
}

fn node_summary_value(nodes: &HashMap<BuildKey, PlannedNode>, key: BuildKey) -> Value {
    let mut object = Map::new();
    object.insert("build_key".to_string(), Value::String(key.to_string()));
    object.insert(
        "short_build_key".to_string(),
        Value::String(short_build_key(key)),
    );
    if let Some(node) = nodes.get(&key) {
        object.insert(
            "tag".to_string(),
            Value::String(node.recipe.tag().to_string()),
        );
        object.insert("name".to_string(), Value::String(node.publish_name.clone()));
    }
    Value::Object(object)
}

fn short_build_key(key: BuildKey) -> String {
    key.to_string().chars().take(12).collect()
}

fn build_resolved_inputs(
    layout: &Store,
    recipe: &PlannedBuilderRecipe,
    completed: &HashMap<BuildKey, RealizedResult>,
) -> Result<ResolvedInputs, RuntimeError> {
    let mut inputs = ResolvedInputs::empty();
    for input_name in recipe.spec.ordered_present_input_names(&recipe.inputs) {
        let key = *recipe.inputs.get(input_name).ok_or_else(|| {
            RuntimeError::Store(format!(
                "planned builder input '{}' is missing for '{}'",
                input_name, recipe.name
            ))
        })?;
        let dep = resolved_dependency_from_completed(layout, completed, key)?;
        inputs.insert(input_name, dep);
    }
    Ok(inputs)
}

fn resolved_dependency_from_completed(
    layout: &Store,
    completed: &HashMap<BuildKey, RealizedResult>,
    key: BuildKey,
) -> Result<ResolvedDependency, RuntimeError> {
    let realized = completed.get(&key).cloned().ok_or_else(|| {
        RuntimeError::Build(format!(
            "dependency result '{}' is not available in completed set",
            key
        ))
    })?;
    Ok(ResolvedDependency {
        object_hash: realized.object_hash,
        object_path: layout.object_path(realized.object_hash),
    })
}

fn execute_builder_recipe(
    layout: &Store,
    logger: Arc<BuildRunLogger>,
    key: BuildKey,
    recipe: PlannedBuilderRecipe,
    cancellation: CancellationToken,
    inputs: ResolvedInputs,
) -> Result<ExecutedNode, RuntimeError> {
    let created_at = logger.created_at().to_string();
    let published = execute_builder_node(ExecuteBuilderNodeRequest {
        layout,
        builder: builders::get_builder(recipe.spec.tag).ok_or_else(|| {
            RuntimeError::UnknownBuilder(format!(
                "unknown builder tag '{}'; supported builders: {}",
                recipe.spec.tag,
                builders::supported_builder_tags().join(", ")
            ))
        })?,
        build_key: key,
        build_name: &recipe.name,
        created_at: &created_at,
        run_logger: logger,
        cancellation,
        config: recipe.config,
        inputs,
    })?;
    Ok(ExecutedNode {
        realized: realized_result_from_record(Some(key), &published.result),
    })
}

fn execute_source_recipe(
    layout: &Store,
    run_logger: Arc<BuildRunLogger>,
    key: BuildKey,
    cancellation: CancellationToken,
    recipe: PlannedSourceRecipe,
) -> Result<ExecutedNode, RuntimeError> {
    let logger = run_logger.bind_node("Source", &recipe.name, key);
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting builder node",
    );
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "materializing source",
    );
    check_cancelled(&cancellation)?;

    match lookup_source_result(layout, recipe.object_hash, run_logger.created_at())
        .map_err(map_store_error)?
    {
        SourceLookup::Hit(stored) => {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Info,
                "result-hit",
                "reusing existing source result",
            );
            return Ok(ExecutedNode {
                realized: realized_result_from_record(None, &stored.result),
            });
        }
        SourceLookup::Missing => {}
    }

    if recipe.origin.is_none() {
        let message = format!(
            "source '{}' has no origin and object '{}' is not present in store",
            recipe.name, recipe.object_hash
        );
        log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
        return Err(RuntimeError::Build(message));
    }

    let temp_root = layout
        .root()
        .join("source-state")
        .join("tmp")
        .join(key.to_hex());
    recreate_store_temp_dir_force(layout, &temp_root).map_err(|error| {
        RuntimeError::Store(format!(
            "failed to prepare source temp dir '{}': {error}",
            temp_root.display()
        ))
    })?;
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_source_temp_dir(layout, &temp_root, logger.as_ref());
        return Err(error);
    }
    let staged_path = match recipe
        .origin
        .as_ref()
        .expect("origin checked above")
        .materialize(&OriginContext {
            temp_root: temp_root.as_path(),
        }) {
        Ok(path) => path,
        Err(error) => {
            cleanup_source_temp_dir(layout, &temp_root, logger.as_ref());
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                error.to_string(),
            );
            return Err(RuntimeError::Build(error));
        }
    };
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_source_temp_dir(layout, &temp_root, logger.as_ref());
        return Err(error);
    }
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "run",
        "materializing source origin",
    );

    let import_outcome = import_source_result(
        layout,
        recipe.object_hash,
        &staged_path,
        run_logger.created_at(),
    )
    .map_err(|error| {
        cleanup_source_temp_dir(layout, &temp_root, logger.as_ref());
        map_store_error(error)
    })?;
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_source_temp_dir(layout, &temp_root, logger.as_ref());
        return Err(error);
    }
    cleanup_source_temp_dir(layout, &temp_root, logger.as_ref());

    match import_outcome {
        SourceImportOutcome::Matched(stored) => Ok(ExecutedNode {
            realized: realized_result_from_record(None, &stored.result),
        }),
        SourceImportOutcome::Mismatched { actual_hash } => {
            let message = format!(
                "source '{}' materialized unexpected object hash: expected {}, got {}",
                recipe.name, recipe.object_hash, actual_hash
            );
            log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
            Err(RuntimeError::Build(message))
        }
    }
}

fn cleanup_source_temp_dir(layout: &Store, temp_dir: &Path, logger: &dyn BuildLogger) {
    if let Err(error) = remove_store_temp_dir_force(layout, temp_dir) {
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

fn realized_result_from_record(
    build_key: Option<BuildKey>,
    result: &ResultRecord,
) -> RealizedResult {
    RealizedResult {
        result_id: result.result_id(),
        build_key,
        object_hash: result.object_hash,
        created_at: result.created_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{ReuseOrigin, collect_graph};
    use mbuild_core::{CancellationToken, OriginContext, OriginSpec, ParsedOrigin};
    use mbuild_store::{
        PublishOutputRequest, compute_result_id, compute_reuse_key, publish_output,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn create_test_store(root: &Path) -> Store {
        let store_root = root.join(".mbuild");
        fs::create_dir_all(&store_root).unwrap();
        Store::create(&store_root).unwrap()
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

    fn sample_realized(build_key: Option<BuildKey>, object_hash: &str) -> RealizedResult {
        let object_hash = object_hash.parse().unwrap();
        RealizedResult {
            result_id: compute_result_id(object_hash).unwrap(),
            build_key,
            object_hash,
            created_at: None,
        }
    }

    #[test]
    fn lookup_canonical_for_planned_node_uses_dependency_object_hashes() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let request = RecipeEnvelope::parse_json(
            br##"{
                "paths": {
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
        let graph = collect_graph(&request, "root", &mut nodes).unwrap();
        let root_key = graph.root_key;
        let dep_keys = {
            let mut keys = Vec::new();
            nodes
                .get(&root_key)
                .unwrap()
                .recipe
                .try_for_each_direct_dep(|dep| {
                    keys.push(dep);
                    Ok::<_, RuntimeError>(())
                })
                .unwrap();
            keys
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
        nodes.get_mut(&dep_keys[0]).unwrap().state = PlanningState::Reused {
            realized: rootfs_realized.clone(),
            origin: ReuseOrigin::CanonicalResult,
        };
        nodes.get_mut(&dep_keys[1]).unwrap().state = PlanningState::Reused {
            realized: script_realized.clone(),
            origin: ReuseOrigin::CanonicalResult,
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
        publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "bin".to_string(),
                build_key: root_key,
                reuse_key,
                created_at: "2026-04-05T12:00:00.000000000Z".to_string(),
                staged_path: stage_dir,
                inputs: root_inputs,
            },
        )
        .unwrap();

        let published = lookup_canonical_for_planned_node(&layout, &nodes, root_key)
            .unwrap()
            .expect("expected canonical result hit");
        assert_eq!(published.build_key, Some(root_key));
    }

    #[test]
    fn source_temp_dir_is_removed_when_cancelled_after_materialize() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let logger = Arc::new(BuildRunLogger::new(layout.root(), RunOptions::default()).unwrap());
        let cancellation = CancellationToken::new();
        let object_hash = "1111111111111111111111111111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let key =
            BuildKey::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();
        let recipe = PlannedSourceRecipe {
            name: "cancel-source".to_string(),
            object_hash,
            origin: Some(Box::new(CancellingOrigin {
                cancellation: cancellation.clone(),
            })),
        };

        let error = execute_source_recipe(&layout, logger, key, cancellation, recipe)
            .expect_err("expected cancellation");

        assert_eq!(error.class(), "cancelled");
        assert!(
            !layout
                .root()
                .join("source-state")
                .join("tmp")
                .join(key.to_hex())
                .exists()
        );
    }
}
