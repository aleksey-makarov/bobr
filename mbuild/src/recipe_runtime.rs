use crate::builders;
use crate::logging::{BuildRunLogger, RunOptions};
use crate::recipe::{
    PlannedInputValue, PlannedNode, PlannedRecipe, PlanningState, Recipe, ReuseOrigin,
    collect_graph,
};
use crate::resolved_inputs::{ResolvedDependencyValue, ResolvedInputs};
use crate::runtime::{
    RuntimeError, build_to_published, execute_builder_node, log_runtime_event,
    lookup_build_handle, lookup_canonical_result, map_store_error, to_resolved_dependency,
    validate_allowed_kind,
};
use mbuild_core::{
    Build, BuildLogEvent, BuildLogLevel, BuildLogger, BuildKey, PublishedBuild, StoreLayout,
    publish_refs,
};
use serde_json::to_string_pretty;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

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

pub fn run_recipe_json_in_workspace(
    workspace_root: &Path,
    recipe_path: &Path,
) -> Result<Build, RuntimeError> {
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
) -> Result<Build, RuntimeError> {
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
    let recipe = Recipe::parse_json(&recipe_bytes).map_err(|error| {
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
    let root_key = collect_graph(&recipe, &mut nodes)?;
    plan_top_down(&recipe, &layout, &mut nodes)?;
    publish_reused_nodes(&layout, logger.as_ref(), &nodes)?;

    let mut completed = HashMap::new();
    for (key, node) in &nodes {
        if let PlanningState::Reused { build, .. } = &node.state {
            completed.insert(*key, build.clone());
        }
    }

    if let Some(build) = completed.get(&root_key) {
        return Ok(build.clone());
    }

    execute_misses(
        workspace_root,
        &layout,
        logger,
        &nodes,
        &mut completed,
        root_key,
        options.jobs,
    )
}

pub fn render_build_as_json(build: &Build) -> Result<String, RuntimeError> {
    let mut rendered = to_string_pretty(build).map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to render Build as JSON: {error}"))
    })?;
    rendered.push('\n');
    Ok(rendered)
}

fn plan_top_down(
    recipe: &Recipe,
    layout: &StoreLayout,
    nodes: &mut HashMap<BuildKey, PlannedNode>,
) -> Result<(), RuntimeError> {
    let key = collect_graph(recipe, nodes)?;
    let node = nodes.get_mut(&key).ok_or_else(|| {
        RuntimeError::Store(format!("missing planned node for build '{}'", key))
    })?;
    node.active_names.insert(recipe.name().to_string());
    match node.state {
        PlanningState::Reused { .. } | PlanningState::NeedsBuild => return Ok(()),
        PlanningState::Unknown => {}
    }

    if let Some(published) = lookup_build_handle(layout, key)? {
        node.state = PlanningState::Reused {
            build: published.build.clone(),
            origin: ReuseOrigin::BuildHandle,
        };
        return Ok(());
    }

    for child in recipe.direct_children() {
        plan_top_down(child, layout, nodes)?;
    }

    if let Some(published) = lookup_canonical_for_planned_node(layout, nodes, key)? {
        let node = nodes.get_mut(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for build '{}'", key))
        })?;
        node.state = PlanningState::Reused {
            build: published.build.clone(),
            origin: ReuseOrigin::CanonicalResult,
        };
    } else {
        let node = nodes.get_mut(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for build '{}'", key))
        })?;
        node.state = PlanningState::NeedsBuild;
    }

    Ok(())
}

fn publish_reused_nodes(
    layout: &StoreLayout,
    logger: &dyn BuildLogger,
    nodes: &HashMap<BuildKey, PlannedNode>,
) -> Result<(), RuntimeError> {
    for node in nodes.values() {
        let PlanningState::Reused { build, origin } = &node.state else {
            continue;
        };
        if node.active_names.is_empty() {
            continue;
        }
        let published = build_to_published(layout, build.clone())?;
        for name in &node.active_names {
            log_runtime_event(
                logger,
                BuildLogLevel::Info,
                "start",
                node.recipe.builder_tag(),
                name,
                build.build_key,
                "starting builder node",
            );
            log_runtime_event(
                logger,
                BuildLogLevel::Info,
                match origin {
                    ReuseOrigin::BuildHandle => "cache-hit",
                    ReuseOrigin::CanonicalResult => "result-hit",
                },
                node.recipe.builder_tag(),
                name,
                build.build_key,
                match origin {
                    ReuseOrigin::BuildHandle => "reusing existing build ref",
                    ReuseOrigin::CanonicalResult => "reusing existing canonical result",
                },
            );
            publish_refs(layout, name, &published).map_err(map_store_error)?;
            log_runtime_event(
                logger,
                BuildLogLevel::Info,
                "publish",
                node.recipe.builder_tag(),
                name,
                build.build_key,
                format!("published '{}' -> {}", name, build.object_hash),
            );
            logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                phase: "done".to_string(),
                builder: node.recipe.builder_tag().to_string(),
                name: name.clone(),
                build_key: build.build_key,
                message: "builder node completed".to_string(),
                object_hash: Some(build.object_hash),
                raw_log_path: None,
                details: serde_json::Map::new(),
            });
        }
    }
    Ok(())
}

fn lookup_canonical_for_planned_node(
    layout: &StoreLayout,
    nodes: &HashMap<BuildKey, PlannedNode>,
    key: BuildKey,
) -> Result<Option<PublishedBuild>, RuntimeError> {
    let node = nodes
        .get(&key)
        .ok_or_else(|| RuntimeError::Store(format!("missing planned node for build '{}'", key)))?;
    let mut input_object_hashes = Vec::new();
    let mut all_reused = true;
    node.recipe.try_for_each_direct_dep(|dep| {
        let dep_node = nodes.get(&dep).ok_or_else(|| {
            RuntimeError::Store(format!("missing dependency node '{}' for build '{}'", dep, key))
        })?;
        match &dep_node.state {
            PlanningState::Reused { build, .. } => input_object_hashes.push(build.object_hash),
            PlanningState::Unknown | PlanningState::NeedsBuild => all_reused = false,
        }
        Ok(())
    })?;
    if !all_reused {
        return Ok(None);
    }
    lookup_canonical_result(
        layout,
        node.recipe.builder_tag(),
        node.recipe.config(),
        &input_object_hashes,
        key,
    )
}

fn execute_misses(
    workspace_root: &Path,
    layout: &StoreLayout,
    logger: Arc<BuildRunLogger>,
    nodes: &HashMap<BuildKey, PlannedNode>,
    completed: &mut HashMap<BuildKey, Build>,
    root_key: BuildKey,
    jobs: usize,
) -> Result<Build, RuntimeError> {
    let mut remaining = HashMap::<BuildKey, usize>::new();
    let mut reverse = HashMap::<BuildKey, Vec<BuildKey>>::new();
    let mut ready = VecDeque::<BuildKey>::new();

    for (key, node) in nodes {
        if !matches!(node.state, PlanningState::NeedsBuild) || node.active_names.is_empty() {
            continue;
        }
        let mut wait_for = 0usize;
        node.recipe.try_for_each_direct_dep(|dep| -> Result<(), RuntimeError> {
            if let Some(dep_node) = nodes.get(&dep) {
                if matches!(dep_node.state, PlanningState::NeedsBuild) && !dep_node.active_names.is_empty() {
                    wait_for += 1;
                    reverse.entry(dep).or_default().push(*key);
                }
            }
            Ok(())
        })?;
        remaining.insert(*key, wait_for);
        if wait_for == 0 {
            ready.push_back(*key);
        }
    }

    let (tx, rx) = mpsc::channel::<(BuildKey, Result<PublishedBuild, RuntimeError>)>();
    let mut in_flight = HashMap::<BuildKey, JoinHandle<()>>::new();

    while !completed.contains_key(&root_key) {
        while in_flight.len() < jobs {
            let Some(key) = ready.pop_front() else {
                break;
            };
            if completed.contains_key(&key) || in_flight.contains_key(&key) {
                continue;
            }
            let node = nodes.get(&key).ok_or_else(|| {
                RuntimeError::Store(format!("missing planned node for build '{}'", key))
            })?;
            let builder = builders::get_builder(node.recipe.builder_tag()).ok_or_else(|| {
                RuntimeError::UnknownBuilder(format!(
                    "unknown builder tag '{}'; supported builders: {}",
                    node.recipe.builder_tag(),
                    builders::supported_builder_tags().join(", ")
                ))
            })?;
            let inputs = build_resolved_inputs(layout, builder, &node.recipe, completed)?;
            let config = node.recipe.config().clone();
            let build_name = node.recipe.build_name().to_string();
            let builder_tag = node.recipe.builder_tag().to_string();
            let workspace_root = workspace_root.to_path_buf();
            let layout = layout.clone();
            let logger = logger.clone();
            let tx = tx.clone();
            let handle = thread::spawn(move || {
                let result = match builders::get_builder(&builder_tag) {
                    Some(builder) => execute_builder_node(
                        &workspace_root,
                        &layout,
                        builder,
                        &build_name,
                        logger.created_at(),
                        logger.clone(),
                        config,
                        inputs,
                    ),
                    None => Err(RuntimeError::UnknownBuilder(format!(
                        "worker failed to resolve builder tag '{}'; supported builders: {}",
                        builder_tag,
                        builders::supported_builder_tags().join(", ")
                    ))),
                };
                let _ = tx.send((key, result));
            });
            in_flight.insert(key, handle);
        }

        if completed.contains_key(&root_key) {
            break;
        }

        if in_flight.is_empty() {
            return Err(RuntimeError::Build(
                "planner/executor stalled: no ready jobs and root build is still unresolved"
                    .to_string(),
            ));
        }

        let (key, result) = rx.recv().map_err(|error| {
            RuntimeError::Build(format!("worker channel closed unexpectedly: {error}"))
        })?;
        if let Some(handle) = in_flight.remove(&key) {
            handle.join().map_err(|_| {
                RuntimeError::Build(format!("worker thread for build '{}' panicked", key))
            })?;
        }
        let published = result?;
        let node = nodes.get(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for build '{}'", key))
        })?;
        for name in &node.active_names {
            publish_refs(layout, name, &published).map_err(map_store_error)?;
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Info,
                "publish",
                node.recipe.builder_tag(),
                name,
                key,
                format!("published '{}' -> {}", name, published.build.object_hash),
            );
            logger.as_ref().log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                phase: "done".to_string(),
                builder: node.recipe.builder_tag().to_string(),
                name: name.clone(),
                build_key: key,
                message: "builder node completed".to_string(),
                object_hash: Some(published.build.object_hash),
                raw_log_path: None,
                details: serde_json::Map::new(),
            });
        }
        completed.insert(key, published.build.clone());
        if let Some(parents) = reverse.get(&key) {
            for parent in parents {
                let pending = remaining.get_mut(parent).ok_or_else(|| {
                    RuntimeError::Store(format!(
                        "missing pending-dependency counter for build '{}'",
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

    completed.get(&root_key).cloned().ok_or_else(|| {
        RuntimeError::Store(format!("root build '{}' is missing after executor completion", root_key))
    })
}

fn build_resolved_inputs(
    layout: &StoreLayout,
    builder: &'static dyn mbuild_core::Builder,
    recipe: &PlannedRecipe,
    completed: &HashMap<BuildKey, Build>,
) -> Result<ResolvedInputs, RuntimeError> {
    let mut inputs = ResolvedInputs::empty();
    for slot in builder.spec().inputs {
        let planned = recipe.inputs().get(slot.name).ok_or_else(|| {
            RuntimeError::InvalidRequest(format!(
                "planned recipe '{}' is missing input slot '{}' for builder '{}'",
                recipe.build_name(),
                slot.name,
                builder.spec().tag
            ))
        })?;
        match (slot.arity, planned) {
            (mbuild_core::InputArity::One, PlannedInputValue::One(key)) => {
                let dep = resolved_dependency_from_completed(layout, completed, *key)?;
                validate_allowed_kind(builder, slot.name, slot.allowed_kinds, &dep.kind)?;
                inputs.insert(slot.name, ResolvedDependencyValue::One(dep));
            }
            (mbuild_core::InputArity::Optional, PlannedInputValue::Optional(maybe_key)) => {
                let dep = match maybe_key {
                    Some(key) => {
                        let dep = resolved_dependency_from_completed(layout, completed, *key)?;
                        validate_allowed_kind(builder, slot.name, slot.allowed_kinds, &dep.kind)?;
                        Some(dep)
                    }
                    None => None,
                };
                inputs.insert(slot.name, ResolvedDependencyValue::Optional(dep));
            }
            (mbuild_core::InputArity::Many, PlannedInputValue::Many(keys)) => {
                let mut deps = Vec::with_capacity(keys.len());
                for key in keys {
                    let dep = resolved_dependency_from_completed(layout, completed, *key)?;
                    validate_allowed_kind(builder, slot.name, slot.allowed_kinds, &dep.kind)?;
                    deps.push(dep);
                }
                inputs.insert(slot.name, ResolvedDependencyValue::Many(deps));
            }
            (arity, _) => {
                return Err(RuntimeError::InvalidRequest(format!(
                    "planned recipe '{}' has unexpected value shape for slot '{}' with arity '{arity:?}'",
                    recipe.build_name(),
                    slot.name,
                )));
            }
        }
    }
    Ok(inputs)
}

fn resolved_dependency_from_completed(
    layout: &StoreLayout,
    completed: &HashMap<BuildKey, Build>,
    key: BuildKey,
) -> Result<crate::resolved_inputs::ResolvedDependency, RuntimeError> {
    let build = completed.get(&key).cloned().ok_or_else(|| {
        RuntimeError::Build(format!(
            "dependency build '{}' is not available in completed set",
            key
        ))
    })?;
    let published = build_to_published(layout, build)?;
    Ok(to_resolved_dependency(published))
}
