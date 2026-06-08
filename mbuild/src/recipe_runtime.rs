use crate::builders;
use crate::recipe::{
    PlannedBuilderRecipe, PlannedNode, PlannedRecipe, PlannedSourceRecipe, PlanningState,
    RecipeEnvelope, RecipePaths, RecipeRequest, ReuseOrigin, collect_graph,
};
use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use crate::runtime::{
    ExecuteBuilderNodeRequest, RuntimeError, check_cancelled, execute_builder_node,
    log_runtime_event, lookup_build_handle, lookup_canonical_object, map_store_error,
};
use bobr_store::identity::BuildKey;
use bobr_store::{
    ObjectRecord, RealizedObject, ReuseInputIdentity, SourceImportOutcome, SourceLookup, Store,
    StoreWorkspace, create_workspace, import_source_object, lookup_source_object,
    publish_stored_object, remove_store_temp_dir_force,
};
use mbuild_core::{
    BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, BuilderClassBase, BuilderRun,
    CancellationToken, OriginContext, RunOptions, SourceBuilderClass, SourceBuilderInit, Workspace,
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
    realized: RealizedObject,
    logger: Arc<dyn BuildLogger>,
}

pub fn run_recipe_json_in_workspace(
    workspace_root: &Path,
    recipe_path: &Path,
) -> Result<RealizedObject, RuntimeError> {
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
    run_recipe_request_in_store_with_options(&envelope.paths, envelope.request, options)
}

pub fn run_recipe_request_in_store_with_options(
    paths: &RecipePaths,
    request: RecipeRequest,
    options: BuildRunOptions,
) -> Result<RealizedObject, RuntimeError> {
    if options.jobs == 0 {
        return Err(RuntimeError::InvalidRequest(
            "--jobs must be greater than zero".to_string(),
        ));
    }
    check_cancelled(&options.cancellation)?;

    let layout = Store::create(&paths.store).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> = Arc::new(
        build_run_logger_for_store(
            &layout,
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
    ensure_planned(&layout, &mut nodes, root_key, layout.created_at())?;

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

pub fn render_object_as_json(object: &RealizedObject) -> Result<String, RuntimeError> {
    let mut rendered = to_string_pretty(object).map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to render realized object as JSON: {error}"))
    })?;
    rendered.push('\n');
    Ok(rendered)
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
                    realized: realized_object_from_record(
                        Some(published.build.build_key),
                        &published.object_record,
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
                    origin: ReuseOrigin::CanonicalObject,
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
            match lookup_source_object(layout, source.object_hash, created_at)
                .map_err(map_store_error)?
            {
                SourceLookup::Hit(stored) => {
                    node.state = PlanningState::Reused {
                        realized: realized_object_from_record(None, &stored.object_record),
                        origin: ReuseOrigin::CanonicalObject,
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
    realized: &RealizedObject,
    origin: ReuseOrigin,
) -> Result<(), RuntimeError> {
    let workspace = create_workspace(
        layout,
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
    publish_stored_object(layout, root_name, realized.object_hash).map_err(map_store_error)?;
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
    cleanup_workspace_temp_dir(layout, root_run.temp_dir(), node_logger.as_ref());
    Ok(())
}

fn lookup_canonical_for_planned_node(
    layout: &Store,
    nodes: &HashMap<BuildKey, PlannedNode>,
    key: BuildKey,
) -> Result<Option<RealizedObject>, RuntimeError> {
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
        lookup_canonical_object(layout, spec.tag, config, &input_identities, key)?
            .map(|published| realized_object_from_record(Some(key), &published.object_record)),
    )
}

fn execute_misses(
    layout: &Store,
    logger: Arc<BuildRunLogger>,
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &mut HashMap<BuildKey, RealizedObject>,
    root_key: BuildKey,
    jobs: usize,
    cancellation: CancellationToken,
) -> Result<RealizedObject, RuntimeError> {
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
    let scheduler_workspace = create_workspace(
        layout,
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
    cleanup_workspace_temp_dir(layout, scheduler_run.temp_dir(), scheduler_logger.as_ref());

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
        publish_stored_object(layout, &node.publish_name, executed.realized.object_hash)
            .map_err(map_store_error)?;
        log_runtime_event(
            executed.logger.as_ref(),
            BuildLogLevel::Info,
            "publish",
            format!(
                "published '{}' -> {}",
                node.publish_name, executed.realized.object_hash
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
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &HashMap<BuildKey, RealizedObject>,
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
    completed: &HashMap<BuildKey, RealizedObject>,
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
    completed: &HashMap<BuildKey, RealizedObject>,
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
    completed: &HashMap<BuildKey, RealizedObject>,
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
    completed: &HashMap<BuildKey, RealizedObject>,
    key: BuildKey,
) -> Result<ResolvedDependency, RuntimeError> {
    let realized = completed.get(&key).cloned().ok_or_else(|| {
        RuntimeError::Build(format!(
            "dependency object '{}' is not available in completed set",
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
    let executed = execute_builder_node(ExecuteBuilderNodeRequest {
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
        run_logger: logger,
        cancellation,
        config: recipe.config,
        inputs,
    })?;
    Ok(ExecutedNode {
        realized: realized_object_from_record(Some(key), &executed.published.object_record),
        logger: executed.logger,
    })
}

fn execute_source_recipe(
    layout: &Store,
    run_logger: Arc<BuildRunLogger>,
    key: BuildKey,
    cancellation: CancellationToken,
    recipe: PlannedSourceRecipe,
) -> Result<ExecutedNode, RuntimeError> {
    let workspace = create_workspace(layout, "Source", Some(recipe.name.clone()), key.to_string())
        .map_err(map_store_error)?;
    let workspace = core_workspace(workspace);
    let source_builder = SourceBuilderClass.create_object(SourceBuilderInit {
        recipe_name: recipe.name,
        build_key: key.to_string(),
        declared_object_hash: recipe.object_hash,
        origin: recipe.origin,
        workspace,
    });
    let logger = run_logger
        .bind_source(&source_builder)
        .map_err(RuntimeError::Store)?;
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting builder node",
    );
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_workspace_temp_dir(layout, source_builder.temp_dir(), logger.as_ref());
        return Err(error);
    }

    match lookup_source_object(
        layout,
        source_builder.declared_object_hash(),
        layout.created_at(),
    )
    .map_err(map_store_error)?
    {
        SourceLookup::Hit(stored) => {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Info,
                "object-hit",
                "reusing existing source object",
            );
            cleanup_workspace_temp_dir(layout, source_builder.temp_dir(), logger.as_ref());
            return Ok(ExecutedNode {
                realized: realized_object_from_record(None, &stored.object_record),
                logger,
            });
        }
        SourceLookup::Missing => {}
    }
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "materializing source",
    );

    if source_builder.origin().is_none() {
        let message = format!(
            "source '{}' has no origin and object '{}' is not present in store",
            source_builder.recipe_name(),
            source_builder.declared_object_hash()
        );
        log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
        cleanup_workspace_temp_dir(layout, source_builder.temp_dir(), logger.as_ref());
        return Err(RuntimeError::Build(message));
    }

    let temp_root = source_builder.temp_dir().to_path_buf();
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_workspace_temp_dir(layout, &temp_root, logger.as_ref());
        return Err(error);
    }
    let staged_path = match source_builder
        .origin()
        .expect("origin checked above")
        .materialize(&OriginContext {
            temp_root: temp_root.as_path(),
        }) {
        Ok(path) => path,
        Err(error) => {
            cleanup_workspace_temp_dir(layout, &temp_root, logger.as_ref());
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
        cleanup_workspace_temp_dir(layout, &temp_root, logger.as_ref());
        return Err(error);
    }
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "run",
        "materializing source origin",
    );

    let import_outcome = import_source_object(
        layout,
        source_builder.declared_object_hash(),
        &staged_path,
        layout.created_at(),
    )
    .map_err(|error| {
        cleanup_workspace_temp_dir(layout, &temp_root, logger.as_ref());
        map_store_error(error)
    })?;
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_workspace_temp_dir(layout, &temp_root, logger.as_ref());
        return Err(error);
    }
    cleanup_workspace_temp_dir(layout, &temp_root, logger.as_ref());

    match import_outcome {
        SourceImportOutcome::Matched(stored) => Ok(ExecutedNode {
            realized: realized_object_from_record(None, &stored.object_record),
            logger,
        }),
        SourceImportOutcome::Mismatched { actual_hash } => {
            let message = format!(
                "source '{}' materialized unexpected object hash: expected {}, got {}",
                source_builder.recipe_name(),
                source_builder.declared_object_hash(),
                actual_hash
            );
            log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
            Err(RuntimeError::Build(message))
        }
    }
}

fn build_run_logger_for_store(
    layout: &Store,
    options: RunOptions,
) -> Result<BuildRunLogger, String> {
    let locations = layout.run_log_locations();
    BuildRunLogger::new(locations.run_log_dir(), locations.created_at(), options)
}

fn core_workspace(workspace: StoreWorkspace) -> Workspace {
    Workspace::new(
        workspace.log_dir().to_path_buf(),
        workspace.raw_log_dir().to_path_buf(),
        workspace.temp_dir().to_path_buf(),
    )
}

fn cleanup_workspace_temp_dir(layout: &Store, temp_dir: &Path, logger: &dyn BuildLogger) {
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

fn realized_object_from_record(
    build_key: Option<BuildKey>,
    object_record: &ObjectRecord,
) -> RealizedObject {
    RealizedObject {
        build_key,
        object_hash: object_record.object_hash,
        created_at: object_record.created_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{ReuseOrigin, collect_graph};
    use bobr_store::identity::compute_reuse_key;
    use bobr_store::{PublishRequest, publish_build};
    use mbuild_core::{CancellationToken, OriginContext, OriginSpec, ParsedOrigin};
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn create_test_store(root: &Path) -> Store {
        let store_root = root.join(".mbuild");
        fs::create_dir_all(&store_root).unwrap();
        Store::create(&store_root).unwrap()
    }

    fn create_test_logger(layout: &Store) -> Arc<BuildRunLogger> {
        Arc::new(build_run_logger_for_store(layout, RunOptions::default()).unwrap())
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
            &layout,
            PublishRequest {
                publication_name: "bin".to_string(),
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
            .expect("expected canonical object hit");
        assert_eq!(published.build_key, Some(root_key));
    }

    #[test]
    fn source_temp_dir_is_removed_when_cancelled_after_materialize() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let logger = create_test_logger(&layout);
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
        let metadata = workspace_metadata(temp.path(), "Source", "cancel-source");
        let temp_dir = PathBuf::from(metadata["temp_dir"].as_str().unwrap());
        assert!(!temp_dir.exists());
    }
}
