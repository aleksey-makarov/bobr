use crate::builders;
use crate::logging::{BuildRunLogger, RunOptions};
use crate::recipe::{
    PlannedBuilderRecipe, PlannedNode, PlannedRecipe, PlannedSourceRecipe, PlanningState,
    RecipeRequest, ReuseOrigin, SourceOrigin, SourcePathMode, collect_graph,
};
use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use crate::runtime::{
    RuntimeError, execute_builder_node, log_runtime_event, lookup_build_handle,
    lookup_canonical_result, map_store_error,
};
use fsobj_hash::hash_path;
use mbuild_core::{
    BuildLogEvent, BuildLogLevel, BuildLogger, BuildKey, RealizedResult,
    ResultInputIdentity, ResultRecord, StoreLayout, compute_meta_hash, compute_result_id, fsutil,
    import_object, load_result_record, object_path, publish_result_refs, store_result_record,
};
use serde_json::to_string_pretty;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use tar::Archive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildRunOptions {
    pub emit_progress: bool,
    pub jobs: usize,
}

impl Default for BuildRunOptions {
    fn default() -> Self {
        Self {
            emit_progress: false,
            jobs: 1,
        }
    }
}

#[derive(Debug, Clone)]
struct ExecutedNode {
    realized: RealizedResult,
    result: ResultRecord,
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
    workspace_root: &Path,
    recipe_path: &Path,
    options: BuildRunOptions,
) -> Result<RealizedResult, RuntimeError> {
    if !recipe_path.exists() {
        return Err(RuntimeError::RecipeLoad(format!(
            "recipe file '{}' does not exist",
            recipe_path.display()
        )));
    }

    if options.jobs == 0 {
        return Err(RuntimeError::InvalidRequest(
            "--jobs must be greater than zero".to_string(),
        ));
    }

    let recipe_bytes = fs::read(recipe_path).map_err(|error| {
        RuntimeError::RecipeLoad(format!(
            "failed to read recipe file '{}': {error}",
            recipe_path.display()
        ))
    })?;
    let request = RecipeRequest::parse_json(&recipe_bytes).map_err(|error| {
        RuntimeError::RecipeLoad(format!(
            "failed to parse recipe JSON '{}': {error}",
            recipe_path.display()
        ))
    })?;

    let layout = StoreLayout::discover(&workspace_root.join(".mbuild")).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> = Arc::new(
        BuildRunLogger::new(
            &layout.root,
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
    ensure_planned(&layout, &mut nodes, root_key)?;

    let mut completed = HashMap::new();
    for (key, node) in &nodes {
        if let PlanningState::Reused { realized, .. } = &node.state {
            completed.insert(*key, realized.clone());
        }
    }

    if let Some(realized) = completed.get(&root_key).cloned() {
        let origin = match &nodes
            .get(&root_key)
            .ok_or_else(|| RuntimeError::Store(format!("missing planned node for key '{}'", root_key)))?
            .state
        {
            PlanningState::Reused { origin, .. } => *origin,
            _ => {
                return Err(RuntimeError::Store(format!(
                    "root key '{}' completed without reused state",
                    root_key
                )))
            }
        };
        publish_reused_root(&layout, &logger, root_key, &root_tag, &root_name, &realized, origin)?;
        return Ok(realized);
    }

    execute_misses(
        &layout,
        logger,
        &nodes,
        &mut completed,
        root_key,
        options.jobs,
    )
}

pub fn render_result_as_json(result: &RealizedResult) -> Result<String, RuntimeError> {
    let mut rendered = to_string_pretty(result).map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to render realized result as JSON: {error}"))
    })?;
    rendered.push('\n');
    Ok(rendered)
}

fn ensure_planned(
    layout: &StoreLayout,
    nodes: &mut HashMap<BuildKey, PlannedNode>,
    key: BuildKey,
) -> Result<(), RuntimeError> {
    let recipe = {
        let node = nodes
            .get(&key)
            .ok_or_else(|| RuntimeError::Store(format!("missing planned node for key '{}'", key)))?;
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
                ensure_planned(layout, nodes, dep)?;
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
            if let Some(result) = load_result_record(layout, source.result_id).map_err(map_store_error)? {
                let published_object = object_path(layout, result.object_hash);
                if !published_object.exists() {
                    return Err(RuntimeError::Store(format!(
                        "result '{}' points to missing object '{}'",
                        result.result_id,
                        published_object.display()
                    )));
                }
                let node = nodes.get_mut(&key).ok_or_else(|| {
                    RuntimeError::Store(format!("missing planned node for key '{}'", key))
                })?;
                node.state = PlanningState::Reused {
                    realized: realized_result_from_record(None, &result),
                    origin: ReuseOrigin::CanonicalResult,
                };
            } else {
                let node = nodes.get_mut(&key).ok_or_else(|| {
                    RuntimeError::Store(format!("missing planned node for key '{}'", key))
                })?;
                node.state = PlanningState::NeedsBuild;
            }
        }
    }

    Ok(())
}

fn publish_reused_root(
    layout: &StoreLayout,
    logger: &Arc<BuildRunLogger>,
    key: BuildKey,
    root_tag: &str,
    root_name: &str,
    realized: &RealizedResult,
    origin: ReuseOrigin,
) -> Result<(), RuntimeError> {
    let result = load_result_record(layout, realized.result_id)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            RuntimeError::Store(format!(
                "missing result '{}' for reused root '{}'",
                realized.result_id, root_name
            ))
        })?;
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
    publish_result_refs(layout, root_name, &result).map_err(map_store_error)?;
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
    layout: &StoreLayout,
    nodes: &HashMap<BuildKey, PlannedNode>,
    key: BuildKey,
) -> Result<Option<RealizedResult>, RuntimeError> {
    let node = nodes
        .get(&key)
        .ok_or_else(|| RuntimeError::Store(format!("missing planned node for key '{}'", key)))?;
    let Some((spec, config, _inputs)) = node.recipe.builder() else {
        return Ok(None);
    };

    let mut input_identities = Vec::<ResultInputIdentity>::new();
    let mut all_reused = true;
    node.recipe.try_for_each_direct_dep(|dep| {
        let dep_node = nodes.get(&dep).ok_or_else(|| {
            RuntimeError::Store(format!(
                "missing dependency node '{}' for key '{}'",
                dep, key
            ))
        })?;
        match &dep_node.state {
            PlanningState::Reused { realized, .. } => input_identities.push(ResultInputIdentity {
                object_hash: realized.object_hash,
                meta_hash: realized.meta_hash,
            }),
            PlanningState::Unknown | PlanningState::NeedsBuild => all_reused = false,
        }
        Ok(())
    })?;
    if !all_reused {
        return Ok(None);
    }

    Ok(lookup_canonical_result(layout, spec.tag, config, &input_identities, key)?
        .map(|published| realized_result_from_record(Some(key), &published.result)))
}

fn execute_misses(
    layout: &StoreLayout,
    logger: Arc<BuildRunLogger>,
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &mut HashMap<BuildKey, RealizedResult>,
    root_key: BuildKey,
    jobs: usize,
) -> Result<RealizedResult, RuntimeError> {
    let mut remaining = HashMap::<BuildKey, usize>::new();
    let mut reverse = HashMap::<BuildKey, Vec<BuildKey>>::new();
    let mut ready = VecDeque::<BuildKey>::new();
    let mut first_error: Option<RuntimeError> = None;

    for (key, node) in nodes {
        if !matches!(node.state, PlanningState::NeedsBuild) || node.active_names.is_empty() {
            continue;
        }
        let mut wait_for = 0usize;
        node.recipe
            .try_for_each_direct_dep(|dep| -> Result<(), RuntimeError> {
                if let Some(dep_node) = nodes.get(&dep)
                    && matches!(dep_node.state, PlanningState::NeedsBuild)
                    && !dep_node.active_names.is_empty()
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

    while !completed.contains_key(&root_key) {
        while first_error.is_none() && in_flight.len() < jobs {
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
            let recipe = node.recipe.clone();
            let builder_inputs = match &recipe {
                PlannedRecipe::Builder(builder_recipe) => {
                    Some(build_resolved_inputs(&layout, builder_recipe, completed)?)
                }
                PlannedRecipe::Source(_) => None,
            };
            let handle = thread::spawn(move || {
                let result = match recipe {
                    PlannedRecipe::Builder(builder_recipe) => {
                        execute_builder_recipe(
                            &layout,
                            logger,
                            key,
                            builder_recipe,
                            builder_inputs.expect("builder inputs must be prepared"),
                        )
                    }
                    PlannedRecipe::Source(source_recipe) => {
                        execute_source_recipe(&layout, logger, key, source_recipe)
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

        let (key, result) = rx.recv().map_err(|error| {
            RuntimeError::Build(format!("worker channel closed unexpectedly: {error}"))
        })?;
        if let Some(handle) = in_flight.remove(&key) {
            handle.join().map_err(|_| {
                RuntimeError::Build(format!("worker thread for key '{}' panicked", key))
            })?;
        }
        let executed = match result {
            Ok(executed) => executed,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
        };
        if first_error.is_some() {
            continue;
        }
        let node = nodes.get(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for key '{}'", key))
        })?;
        for name in &node.active_names {
            let node_logger = logger.bind_node(node.recipe.tag(), name, key);
            publish_result_refs(layout, name, &executed.result).map_err(map_store_error)?;
            log_runtime_event(
                node_logger.as_ref(),
                BuildLogLevel::Info,
                "publish",
                format!("published '{}' -> {}", name, executed.realized.object_hash),
            );
            node_logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                phase: "done".to_string(),
                message: "builder node completed".to_string(),
                object_hash: Some(executed.realized.object_hash),
                raw_log_path: None,
                details: serde_json::Map::new(),
            });
        }
        completed.insert(key, executed.realized);
        if let Some(parents) = reverse.get(&key) {
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

fn build_resolved_inputs(
    layout: &StoreLayout,
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
    layout: &StoreLayout,
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
        meta_hash: realized.meta_hash,
        object_path: object_path(layout, realized.object_hash),
        meta: realized.meta,
    })
}

fn execute_builder_recipe(
    layout: &StoreLayout,
    logger: Arc<BuildRunLogger>,
    key: BuildKey,
    recipe: PlannedBuilderRecipe,
    inputs: ResolvedInputs,
) -> Result<ExecutedNode, RuntimeError> {
    let created_at = logger.created_at().to_string();
    let published = execute_builder_node(
        layout,
        builders::get_builder(recipe.spec.tag).ok_or_else(|| {
            RuntimeError::UnknownBuilder(format!(
                "unknown builder tag '{}'; supported builders: {}",
                recipe.spec.tag,
                builders::supported_builder_tags().join(", ")
            ))
        })?,
        key,
        &recipe.name,
        &created_at,
        logger,
        recipe.config,
        inputs,
    )?;
    Ok(ExecutedNode {
        realized: realized_result_from_record(Some(key), &published.result),
        result: published.result,
    })
}

fn execute_source_recipe(
    layout: &StoreLayout,
    run_logger: Arc<BuildRunLogger>,
    key: BuildKey,
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

    if let Some(result) = load_result_record(layout, recipe.result_id).map_err(map_store_error)? {
        let object_path = object_path(layout, result.object_hash);
        if object_path.exists() {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Info,
                "result-hit",
                "reusing existing canonical result",
            );
            return Ok(ExecutedNode {
                realized: realized_result_from_record(None, &result),
                result,
            });
        }
        return Err(RuntimeError::Store(format!(
            "result '{}' points to missing object '{}'",
            result.result_id,
            object_path.display()
        )));
    }

    let existing_object_path = object_path(layout, recipe.object_hash);
    if existing_object_path.exists() {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "run",
            "reusing existing object and publishing source result",
        );
        let result = make_source_result_record(
            recipe.object_hash,
            run_logger.created_at(),
            recipe.meta.clone(),
        )?;
        store_result_record(layout, &result).map_err(map_store_error)?;
        return Ok(ExecutedNode {
            realized: realized_result_from_record(None, &result),
            result,
        });
    }

    let temp_root = layout.root.join("source-state").join("tmp").join(key.to_hex());
    fsutil::recreate_empty_dir_force(&temp_root).map_err(|error| {
        RuntimeError::Store(format!(
            "failed to prepare source temp dir '{}': {error}",
            temp_root.display()
        ))
    })?;
    let staged_path = match materialize_source_origin(&temp_root, &recipe.origin) {
        Ok(path) => path,
        Err(error) => {
            cleanup_source_temp_dir(&temp_root, logger.as_ref());
            log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", error.to_string());
            return Err(error);
        }
    };
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "run",
        "materializing source origin",
    );

    let actual_hash = hash_path(&staged_path).map_err(|error| {
        cleanup_source_temp_dir(&temp_root, logger.as_ref());
        RuntimeError::Build(format!(
            "failed to hash materialized source '{}': {error}",
            staged_path.display()
        ))
    })?;
    if actual_hash != recipe.object_hash {
        cleanup_source_temp_dir(&temp_root, logger.as_ref());
        let message = format!(
            "source '{}' materialized unexpected object hash: expected {}, got {}",
            recipe.name, recipe.object_hash, actual_hash
        );
        log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
        return Err(RuntimeError::Build(message));
    }

    let imported_hash = import_object(layout, &staged_path).map_err(|error| {
        cleanup_source_temp_dir(&temp_root, logger.as_ref());
        map_store_error(error)
    })?;
    cleanup_source_temp_dir(&temp_root, logger.as_ref());
    debug_assert_eq!(imported_hash, recipe.object_hash);

    let result = make_source_result_record(
        recipe.object_hash,
        run_logger.created_at(),
        recipe.meta.clone(),
    )?;
    store_result_record(layout, &result).map_err(map_store_error)?;
    Ok(ExecutedNode {
        realized: realized_result_from_record(None, &result),
        result,
    })
}

fn materialize_source_origin(
    temp_root: &Path,
    origin: &SourceOrigin,
) -> Result<PathBuf, RuntimeError> {
    match origin {
        SourceOrigin::Path { path, mode } => match mode {
            SourcePathMode::Direct => materialize_path_source_direct(temp_root, path),
            SourcePathMode::Tar => materialize_path_source_tar(temp_root, path),
        },
    }
}

fn materialize_path_source_direct(temp_root: &Path, source_path: &Path) -> Result<PathBuf, RuntimeError> {
    let source_meta = fs::metadata(source_path).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to inspect source path '{}': {error}",
            source_path.display()
        ))
    })?;
    let staged_path = temp_root.join("staged");
    if source_meta.is_dir() {
        copy_dir_recursive(source_path, &staged_path)?;
    } else if source_meta.is_file() {
        if let Some(parent) = staged_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RuntimeError::Build(format!(
                    "failed to create staging parent '{}': {error}",
                    parent.display()
                ))
            })?;
        }
        fs::copy(source_path, &staged_path).map_err(|error| {
            RuntimeError::Build(format!(
                "failed to copy source file '{}' to '{}': {error}",
                source_path.display(),
                staged_path.display()
            ))
        })?;
    } else {
        return Err(RuntimeError::Build(format!(
            "source path '{}' must be a regular file or directory",
            source_path.display()
        )));
    }
    Ok(staged_path)
}

fn materialize_path_source_tar(temp_root: &Path, source_path: &Path) -> Result<PathBuf, RuntimeError> {
    let file = fs::File::open(source_path).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to open tar source '{}': {error}",
            source_path.display()
        ))
    })?;
    let staged_path = temp_root.join("staged");
    fs::create_dir_all(&staged_path).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to create tar staging dir '{}': {error}",
            staged_path.display()
        ))
    })?;
    let mut archive = Archive::new(file);
    archive.unpack(&staged_path).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to unpack tar source '{}' into '{}': {error}",
            source_path.display(),
            staged_path.display()
        ))
    })?;
    Ok(staged_path)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), RuntimeError> {
    fs::create_dir_all(dst).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to create directory '{}': {error}",
            dst.display()
        ))
    })?;
    for entry in fs::read_dir(src).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to read directory '{}': {error}",
            src.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            RuntimeError::Build(format!(
                "failed to read entry under '{}': {error}",
                src.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            RuntimeError::Build(format!(
                "failed to inspect '{}' file type: {error}",
                entry.path().display()
            ))
        })?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).map_err(|error| {
                RuntimeError::Build(format!(
                    "failed to copy '{}' to '{}': {error}",
                    entry.path().display(),
                    target.display()
                ))
            })?;
        } else if file_type.is_symlink() {
            copy_symlink(entry.path().as_path(), &target)?;
        } else {
            return Err(RuntimeError::Build(format!(
                "unsupported source entry '{}'",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs as unix_fs;
    let target = fs::read_link(src).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to read symlink '{}': {error}",
            src.display()
        ))
    })?;
    unix_fs::symlink(&target, dst).map_err(|error| {
        RuntimeError::Build(format!(
            "failed to create symlink '{}' -> '{}': {error}",
            dst.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn copy_symlink(_src: &Path, _dst: &Path) -> Result<(), RuntimeError> {
    Err(RuntimeError::Build(
        "copying symlink entries is unsupported on this platform".to_string(),
    ))
}

fn cleanup_source_temp_dir(temp_dir: &Path, logger: &dyn BuildLogger) {
    if let Err(error) = fsutil::remove_dir_force(temp_dir) {
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

fn make_source_result_record(
    object_hash: fsobj_hash::ObjectHash,
    created_at: &str,
    meta: serde_json::Map<String, serde_json::Value>,
) -> Result<ResultRecord, RuntimeError> {
    let meta_hash = compute_meta_hash(&meta).map_err(map_store_error)?;
    let result_id = compute_result_id(object_hash, meta_hash).map_err(map_store_error)?;
    Ok(ResultRecord {
        result_id,
        object_hash,
        meta_hash,
        created_at: Some(created_at.to_string()),
        inputs: Vec::new(),
        meta,
    })
}

fn realized_result_from_record(
    build_key: Option<BuildKey>,
    result: &ResultRecord,
) -> RealizedResult {
    RealizedResult {
        result_id: result.result_id,
        build_key,
        object_hash: result.object_hash,
        meta_hash: result.meta_hash,
        created_at: result.created_at.clone(),
        meta: result.meta.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{ReuseOrigin, collect_graph};
    use mbuild_core::{MetaHash, PublishOutputRequest, compute_reuse_key, publish_output};
    use serde_json::{Map, json};
    use std::collections::HashMap;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn sample_realized(
        build_key: Option<BuildKey>,
        object_hash: &str,
        meta_hash: &str,
        meta: Map<String, serde_json::Value>,
    ) -> RealizedResult {
        RealizedResult {
            result_id: compute_result_id(
                object_hash.parse().unwrap(),
                MetaHash::from_str(meta_hash).unwrap(),
            )
            .unwrap(),
            build_key,
            object_hash: object_hash.parse().unwrap(),
            meta_hash: MetaHash::from_str(meta_hash).unwrap(),
            created_at: None,
            meta,
        }
    }

    #[test]
    fn lookup_canonical_for_planned_node_uses_dependency_meta_hashes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let request = RecipeRequest::parse_json(
            br##"{
                "root": {
                    "name": "bin",
                    "tag": "Binary",
                    "config": {},
                    "inputs": {
                        "image": "image",
                        "script": "script"
                    }
                },
                "image": {
                    "name": "image",
                    "tag": "ContainerImage",
                    "config": {
                        "image": "docker.io/library/buildpack-deps:bookworm",
                        "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    },
                    "inputs": {}
                },
                "script": {
                    "name": "script",
                    "tag": "Text",
                    "config": {
                        "source": "#!/bin/sh\nexit 0\n",
                        "executable": true
                    },
                    "inputs": {}
                }
            }"##,
        )
        .unwrap();

        let mut nodes = HashMap::new();
        let graph = collect_graph(&request, "root", &mut nodes).unwrap();
        let root_key = graph.root_key;
        let dep_keys = {
            let mut keys = Vec::new();
            nodes.get(&root_key)
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

        let image_realized = sample_realized(
            Some(dep_keys[0]),
            "1111111111111111111111111111111111111111111111111111111111111111",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Map::from_iter([(
                "manifest_digest".to_string(),
                serde_json::Value::String(
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                ),
            )]),
        );
        let script_realized = sample_realized(
            Some(dep_keys[1]),
            "2222222222222222222222222222222222222222222222222222222222222222",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            Map::new(),
        );
        nodes.get_mut(&dep_keys[0]).unwrap().state = PlanningState::Reused {
            realized: image_realized.clone(),
            origin: ReuseOrigin::CanonicalResult,
        };
        nodes.get_mut(&dep_keys[1]).unwrap().state = PlanningState::Reused {
            realized: script_realized.clone(),
            origin: ReuseOrigin::CanonicalResult,
        };

        let root_inputs = vec![
            ResultInputIdentity {
                object_hash: image_realized.object_hash,
                meta_hash: image_realized.meta_hash,
            },
            ResultInputIdentity {
                object_hash: script_realized.object_hash,
                meta_hash: script_realized.meta_hash,
            },
        ];
        let reuse_key = compute_reuse_key("Binary", &json!({}), &root_inputs).unwrap();
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
                meta: Map::new(),
            },
        )
        .unwrap();

        let published = lookup_canonical_for_planned_node(&layout, &nodes, root_key)
            .unwrap()
            .expect("expected canonical result hit");
        assert_eq!(published.build_key, Some(root_key));
    }
}
