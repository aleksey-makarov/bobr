use crate::builders;
use crate::logging::{BuildRunLogger, RunOptions};
use crate::recipe::{
    CollectedGraph, PlannedInputValue, PlannedNode, PlannedRecipe, PlanningState, RecipeRequest,
    ReuseOrigin, collect_graph,
};
use crate::resolved_inputs::{ResolvedDependencyValue, ResolvedInputs};
use crate::runtime::{
    RuntimeError, build_to_published, execute_builder_node, log_runtime_event, lookup_build_handle,
    lookup_canonical_result, map_store_error, to_resolved_dependency,
};
use mbuild_core::{
    Build, BuildKey, BuildLogEvent, BuildLogLevel, PublishedBuild, ResultInputIdentity,
    StoreLayout, publish_refs,
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
    plan_top_down(&layout, &mut nodes, &graph)?;
    publish_reused_nodes(&layout, &logger, &nodes)?;

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
    layout: &StoreLayout,
    nodes: &mut HashMap<BuildKey, PlannedNode>,
    graph: &CollectedGraph,
) -> Result<(), RuntimeError> {
    for node_id in &graph.topo_order {
        let key = *graph.node_keys.get(node_id).ok_or_else(|| {
            RuntimeError::Store(format!(
                "missing build key for request node id '{}'",
                node_id
            ))
        })?;
        let node = nodes.get_mut(&key).ok_or_else(|| {
            RuntimeError::Store(format!("missing planned node for build '{}'", key))
        })?;
        if !matches!(node.state, PlanningState::Unknown) {
            continue;
        }

        if let Some(published) = lookup_build_handle(layout, key)? {
            node.state = PlanningState::Reused {
                build: published.build.clone(),
                origin: ReuseOrigin::BuildHandle,
            };
            continue;
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
    }

    Ok(())
}

fn publish_reused_nodes(
    layout: &StoreLayout,
    logger: &Arc<BuildRunLogger>,
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
            let node_logger = logger.bind_node(node.recipe.builder_tag(), name, build.build_key);
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
            publish_refs(layout, name, &published).map_err(map_store_error)?;
            log_runtime_event(
                node_logger.as_ref(),
                BuildLogLevel::Info,
                "publish",
                format!("published '{}' -> {}", name, build.object_hash),
            );
            node_logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                phase: "done".to_string(),
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
    let mut inputs = Vec::<ResultInputIdentity>::new();
    let mut all_reused = true;
    node.recipe.try_for_each_direct_dep(|dep| {
        let dep_node = nodes.get(&dep).ok_or_else(|| {
            RuntimeError::Store(format!(
                "missing dependency node '{}' for build '{}'",
                dep, key
            ))
        })?;
        match &dep_node.state {
            PlanningState::Reused { build, .. } => inputs.push(ResultInputIdentity {
                object_hash: build.object_hash,
                meta_hash: build.meta_hash,
            }),
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
        &inputs,
        key,
    )
}

fn execute_misses(
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
        node.recipe
            .try_for_each_direct_dep(|dep| -> Result<(), RuntimeError> {
                if let Some(dep_node) = nodes.get(&dep) {
                    if matches!(dep_node.state, PlanningState::NeedsBuild)
                        && !dep_node.active_names.is_empty()
                    {
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
            let layout = layout.clone();
            let logger = logger.clone();
            let tx = tx.clone();
            let handle = thread::spawn(move || {
                let result = match builders::get_builder(&builder_tag) {
                    Some(builder) => execute_builder_node(
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
            let node_logger = logger.bind_node(node.recipe.builder_tag(), name, key);
            publish_refs(layout, name, &published).map_err(map_store_error)?;
            log_runtime_event(
                node_logger.as_ref(),
                BuildLogLevel::Info,
                "publish",
                format!("published '{}' -> {}", name, published.build.object_hash),
            );
            node_logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                phase: "done".to_string(),
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
        RuntimeError::Store(format!(
            "root build '{}' is missing after executor completion",
            root_key
        ))
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
                inputs.insert(slot.name, ResolvedDependencyValue::One(dep));
            }
            (mbuild_core::InputArity::Optional, PlannedInputValue::Optional(maybe_key)) => {
                let dep = match maybe_key {
                    Some(key) => {
                        let dep = resolved_dependency_from_completed(layout, completed, *key)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{ReuseOrigin, collect_graph};
    use mbuild_core::{MetaHash, PublishOutputRequest, publish_output};
    use serde_json::{Map, json};
    use std::collections::HashMap;
    use std::fs;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn sample_build(
        build_key: BuildKey,
        object_hash: &str,
        meta_hash: &str,
        meta: Map<String, serde_json::Value>,
    ) -> Build {
        Build {
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
                        "in": ["script"]
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

        let image_build = sample_build(
            dep_keys[0],
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
        let script_build = sample_build(
            dep_keys[1],
            "2222222222222222222222222222222222222222222222222222222222222222",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            Map::new(),
        );
        nodes.get_mut(&dep_keys[0]).unwrap().state = PlanningState::Reused {
            build: image_build.clone(),
            origin: ReuseOrigin::CanonicalResult,
        };
        nodes.get_mut(&dep_keys[1]).unwrap().state = PlanningState::Reused {
            build: script_build.clone(),
            origin: ReuseOrigin::CanonicalResult,
        };

        let root_inputs = vec![
            ResultInputIdentity {
                object_hash: image_build.object_hash,
                meta_hash: image_build.meta_hash,
            },
            ResultInputIdentity {
                object_hash: script_build.object_hash,
                meta_hash: script_build.meta_hash,
            },
        ];
        let result_key =
            mbuild_core::compute_result_key("Binary", &json!({}), &root_inputs).unwrap();
        let stage = temp.path().join("binary-out");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("tool"), b"payload\n").unwrap();
        publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "bin".to_string(),
                build_key: BuildKey::from_str(
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                )
                .unwrap(),
                result_key,
                created_at: "2026-04-05T12:00:00.000000000Z".to_string(),
                staged_path: stage,
                inputs: root_inputs,
                meta: Map::from_iter([(
                    "install".to_string(),
                    serde_json::json!({"rules":[{"path":"**","attrs":{"uid":0,"gid":0,"directory_mode":493,"regular_file_mode":420,"executable_file_mode":493,"symlink_mode":511}}]}),
                )]),
            },
        )
        .unwrap();

        assert!(
            lookup_canonical_for_planned_node(&layout, &nodes, root_key)
                .unwrap()
                .is_some()
        );

        let script_node = nodes.get_mut(&dep_keys[1]).unwrap();
        if let PlanningState::Reused { build, .. } = &mut script_node.state {
            build.meta_hash = MetaHash::from_str(
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            )
            .unwrap();
        }

        assert!(
            lookup_canonical_for_planned_node(&layout, &nodes, root_key)
                .unwrap()
                .is_none()
        );
    }
}
