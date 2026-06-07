use crate::builders;
use crate::origins;
use crate::runtime::{RuntimeError, map_store_error};
use bobr_store::{BuildKey, RealizedResult, ResultId, compute_build_key, compute_result_id};
use mbuild_core::{BuilderSpec, ObjectHash, ParsedOrigin};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RecipeEnvelope {
    pub paths: RecipePaths,
    pub options: RecipeOptions,
    pub request: RecipeRequest,
}

#[derive(Debug, Clone)]
pub struct RecipePaths {
    pub store: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct RecipeOptions {
    pub quiet: Option<bool>,
    pub jobs: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RecipeRequest {
    nodes: BTreeMap<String, Recipe>,
}

#[derive(Debug, Clone)]
pub enum Recipe {
    Builder(BuilderRecipe),
    Source(SourceRecipe),
}

impl Recipe {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Builder(recipe) => &recipe.name,
            Self::Source(recipe) => &recipe.name,
        }
    }

    pub(crate) fn tag(&self) -> &str {
        match self {
            Self::Builder(recipe) => &recipe.tag,
            Self::Source(_) => "Source",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuilderRecipe {
    name: String,
    tag: String,
    config: Value,
    inputs: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SourceRecipe {
    name: String,
    object_hash: ObjectHash,
    origin: Option<Box<dyn ParsedOrigin>>,
}

impl RecipeEnvelope {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            RuntimeError::RecipeLoad(format!("failed to decode recipe JSON value: {error}"))
        })?;
        parse_envelope_value(value, "$")
    }
}

impl RecipeRequest {
    pub(crate) fn node(&self, id: &str) -> Result<&Recipe, RuntimeError> {
        self.nodes.get(id).ok_or_else(|| {
            RuntimeError::InvalidRequest(format!("request references unknown node id '{id}'"))
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PlannedRecipe {
    Builder(PlannedBuilderRecipe),
    Source(PlannedSourceRecipe),
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedBuilderRecipe {
    pub(crate) name: String,
    pub(crate) spec: &'static BuilderSpec,
    pub(crate) config: Value,
    pub(crate) inputs: BTreeMap<String, BuildKey>,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedSourceRecipe {
    pub(crate) name: String,
    pub(crate) object_hash: ObjectHash,
    pub(crate) origin: Option<Box<dyn ParsedOrigin>>,
}

impl PlannedRecipe {
    pub(crate) fn tag(&self) -> &str {
        match self {
            Self::Builder(recipe) => recipe.spec.tag,
            Self::Source(_) => "Source",
        }
    }

    pub(crate) fn builder(
        &self,
    ) -> Option<(&'static BuilderSpec, &Value, &BTreeMap<String, BuildKey>)> {
        match self {
            Self::Builder(recipe) => Some((recipe.spec, &recipe.config, &recipe.inputs)),
            Self::Source(_) => None,
        }
    }

    pub(crate) fn try_for_each_direct_dep<E>(
        &self,
        mut f: impl FnMut(BuildKey) -> Result<(), E>,
    ) -> Result<(), E> {
        if let Self::Builder(recipe) = self {
            for name in recipe.spec.ordered_present_input_names(&recipe.inputs) {
                let key = recipe
                    .inputs
                    .get(name)
                    .expect("planned recipe inputs must match builder spec");
                f(*key)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedNode {
    pub(crate) recipe: PlannedRecipe,
    pub(crate) publish_name: String,
    pub(crate) state: PlanningState,
}

impl PlannedNode {
    pub(crate) fn new(recipe: PlannedRecipe, publish_name: String) -> Self {
        Self {
            recipe,
            publish_name,
            state: PlanningState::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PlanningState {
    Unknown,
    Reused {
        realized: RealizedResult,
        origin: ReuseOrigin,
    },
    NeedsBuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReuseOrigin {
    BuildHandle,
    CanonicalResult,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct CollectedGraph {
    pub(crate) root_key: BuildKey,
    pub(crate) node_keys: HashMap<String, BuildKey>,
    pub(crate) topo_order: Vec<String>,
}

pub(crate) fn collect_graph(
    request: &RecipeRequest,
    node_id: &str,
    nodes: &mut HashMap<BuildKey, PlannedNode>,
) -> Result<CollectedGraph, RuntimeError> {
    builders::validate_registered_builders().map_err(RuntimeError::InvalidRequest)?;

    let mut stack = BTreeSet::new();
    let mut node_keys = HashMap::new();
    let mut topo_order = Vec::new();
    let root_key = collect_graph_inner(
        request,
        node_id,
        nodes,
        &mut stack,
        &mut node_keys,
        &mut topo_order,
    )?;
    Ok(CollectedGraph {
        root_key,
        node_keys,
        topo_order,
    })
}

fn collect_graph_inner(
    request: &RecipeRequest,
    node_id: &str,
    nodes: &mut HashMap<BuildKey, PlannedNode>,
    stack: &mut BTreeSet<String>,
    node_keys: &mut HashMap<String, BuildKey>,
    topo_order: &mut Vec<String>,
) -> Result<BuildKey, RuntimeError> {
    if let Some(existing) = node_keys.get(node_id) {
        return Ok(*existing);
    }

    if !stack.insert(node_id.to_string()) {
        return Err(RuntimeError::InvalidRequest(format!(
            "request graph contains a cycle through node id '{node_id}'"
        )));
    }

    let recipe = request.node(node_id)?;
    let (key, planned_recipe) = match recipe {
        Recipe::Builder(recipe) => {
            collect_builder_recipe(request, recipe, nodes, stack, node_keys, topo_order)?
        }
        Recipe::Source(recipe) => {
            let result_id = compute_result_id(recipe.object_hash);
            let key = source_planning_key(result_id)?;
            let planned = PlannedRecipe::Source(PlannedSourceRecipe {
                name: recipe.name.clone(),
                object_hash: recipe.object_hash,
                origin: recipe.origin.clone(),
            });
            (key, planned)
        }
    };

    stack.remove(node_id);

    let publish_name = match recipe {
        Recipe::Builder(recipe) => recipe.name.clone(),
        Recipe::Source(recipe) => recipe.name.clone(),
    };
    nodes
        .entry(key)
        .or_insert_with(|| PlannedNode::new(planned_recipe, publish_name));
    node_keys.insert(node_id.to_string(), key);
    topo_order.push(node_id.to_string());
    Ok(key)
}

fn collect_builder_recipe(
    request: &RecipeRequest,
    recipe: &BuilderRecipe,
    _nodes: &mut HashMap<BuildKey, PlannedNode>,
    stack: &mut BTreeSet<String>,
    node_keys: &mut HashMap<String, BuildKey>,
    topo_order: &mut Vec<String>,
) -> Result<(BuildKey, PlannedRecipe), RuntimeError> {
    let builder = builders::get_builder(&recipe.tag).ok_or_else(|| {
        RuntimeError::UnknownBuilder(format!(
            "unknown builder tag '{}'; supported builders: {}",
            recipe.tag,
            builders::supported_builder_tags().join(", ")
        ))
    })?;
    let spec = builder.spec();

    let reserved_inputs = spec.reserved_input_names().collect::<Vec<_>>();
    for input_name in recipe.inputs.keys() {
        if !spec.allow_extra_inputs && !spec.is_reserved_input(input_name) {
            return Err(RuntimeError::InvalidRequest(format!(
                "builder '{}' does not accept extra input '{}'; allowed inputs: {}",
                spec.tag,
                input_name,
                reserved_inputs.join(", ")
            )));
        }
    }

    let mut inputs = BTreeMap::new();
    for required in spec.required_inputs {
        if !recipe.inputs.contains_key(*required) {
            return Err(RuntimeError::InvalidRequest(format!(
                "builder '{}' is missing required input '{}' in recipe '{}'",
                spec.tag, required, recipe.name
            )));
        }
    }

    let mut ordered_direct_deps = Vec::new();
    for (input_name, child_id) in &recipe.inputs {
        let key = collect_graph_inner(request, child_id, _nodes, stack, node_keys, topo_order)?;
        inputs.insert(input_name.clone(), key);
    }
    for input_name in spec.ordered_present_input_names(&inputs) {
        if let Some(key) = inputs.get(input_name) {
            ordered_direct_deps.push(*key);
        }
    }

    let key = compute_build_key(spec.tag, &recipe.config, &ordered_direct_deps)
        .map_err(map_store_error)?;
    Ok((
        key,
        PlannedRecipe::Builder(PlannedBuilderRecipe {
            name: recipe.name.clone(),
            spec,
            config: recipe.config.clone(),
            inputs,
        }),
    ))
}

fn source_planning_key(result_id: ResultId) -> Result<BuildKey, RuntimeError> {
    compute_build_key(
        "SourceNode",
        &json!({
            "result_id": result_id.to_string(),
        }),
        &[],
    )
    .map_err(map_store_error)
}

fn parse_envelope_value(value: Value, path: &str) -> Result<RecipeEnvelope, RuntimeError> {
    let mut object = value.as_object().cloned().ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: expected top-level recipe object"))
    })?;

    let paths = parse_paths_value(
        object.remove("paths").ok_or_else(|| {
            RuntimeError::RecipeLoad(format!("{path}: missing required field 'paths'"))
        })?,
        &format!("{path}.paths"),
    )?;
    let options = match object.remove("options") {
        Some(value) => parse_options_value(value, &format!("{path}.options"))?,
        None => RecipeOptions::default(),
    };
    let request = parse_request_value(
        object.remove("nodes").ok_or_else(|| {
            RuntimeError::RecipeLoad(format!("{path}: missing required field 'nodes'"))
        })?,
        &format!("{path}.nodes"),
    )?;
    if !object.is_empty() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    Ok(RecipeEnvelope {
        paths,
        options,
        request,
    })
}

fn parse_paths_value(value: Value, path: &str) -> Result<RecipePaths, RuntimeError> {
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}: expected object")))?;
    let store = PathBuf::from(take_string(&mut object, path, "store")?);
    if !store.is_absolute() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}.store: expected absolute path"
        )));
    }
    if !object.is_empty() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(RecipePaths { store })
}

fn parse_options_value(value: Value, path: &str) -> Result<RecipeOptions, RuntimeError> {
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}: expected object")))?;

    let quiet = match object.remove("quiet") {
        Some(Value::Bool(value)) => Some(value),
        Some(_) => {
            return Err(RuntimeError::RecipeLoad(format!(
                "{path}.quiet: expected boolean"
            )));
        }
        None => None,
    };
    let jobs = match object.remove("jobs") {
        Some(Value::Number(value)) => {
            let jobs = value.as_u64().ok_or_else(|| {
                RuntimeError::RecipeLoad(format!("{path}.jobs: expected non-negative integer"))
            })?;
            let jobs = usize::try_from(jobs).map_err(|_| {
                RuntimeError::RecipeLoad(format!(
                    "{path}.jobs: value is too large for this platform"
                ))
            })?;
            Some(jobs)
        }
        Some(_) => {
            return Err(RuntimeError::RecipeLoad(format!(
                "{path}.jobs: expected integer"
            )));
        }
        None => None,
    };
    if !object.is_empty() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(RecipeOptions { quiet, jobs })
}

fn parse_request_value(value: Value, path: &str) -> Result<RecipeRequest, RuntimeError> {
    let object = value.as_object().cloned().ok_or_else(|| {
        RuntimeError::RecipeLoad(format!(
            "{path}: expected top-level object of node definitions"
        ))
    })?;

    if !object.contains_key("root") {
        return Err(RuntimeError::RecipeLoad(
            "missing required top-level node 'root'".to_string(),
        ));
    }

    let mut nodes = BTreeMap::new();
    for (node_id, node_value) in object {
        let node_path = format!("{path}.{node_id}");
        nodes.insert(node_id, parse_recipe_value(node_value, &node_path)?);
    }

    Ok(RecipeRequest { nodes })
}

fn parse_recipe_value(value: Value, path: &str) -> Result<Recipe, RuntimeError> {
    let object = value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}: expected recipe object")))?;

    let tag = object
        .get("tag")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}.tag: expected string")))?;

    if tag == "Source" {
        return parse_source_recipe(object, path);
    }
    parse_builder_recipe(object, path)
}

fn parse_builder_recipe(
    mut object: Map<String, Value>,
    path: &str,
) -> Result<Recipe, RuntimeError> {
    let name = take_string(&mut object, path, "name")?;
    let tag = take_string(&mut object, path, "tag")?;
    let config = object.remove("config").ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: missing required field 'config'"))
    })?;
    let inputs_value = object.remove("inputs").ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: missing required field 'inputs'"))
    })?;
    if !object.is_empty() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    let inputs_object = inputs_value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}.inputs: expected object")))?;
    let mut inputs = BTreeMap::new();
    for (slot_name, slot_value) in inputs_object {
        validate_input_name(&slot_name, &format!("{path}.inputs"))?;
        let input_path = format!("{path}.inputs.{slot_name}");
        inputs.insert(slot_name, parse_input_value(slot_value, &input_path)?);
    }

    Ok(Recipe::Builder(BuilderRecipe {
        name,
        tag,
        config,
        inputs,
    }))
}

fn parse_source_recipe(mut object: Map<String, Value>, path: &str) -> Result<Recipe, RuntimeError> {
    let name = take_string(&mut object, path, "name")?;
    let tag = take_string(&mut object, path, "tag")?;
    debug_assert_eq!(tag, "Source");

    let object_hash = take_string(&mut object, path, "object_hash")?
        .trim()
        .parse::<ObjectHash>()
        .map_err(|error| {
            RuntimeError::RecipeLoad(format!("{path}.object_hash: invalid object hash: {error}"))
        })?;
    let origin = match object.remove("origin") {
        Some(value) => Some(origins::parse_origin_value(
            value,
            &format!("{path}.origin"),
        )?),
        None => None,
    };
    if !object.is_empty() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    Ok(Recipe::Source(SourceRecipe {
        name,
        object_hash,
        origin,
    }))
}

fn parse_input_value(value: Value, path: &str) -> Result<String, RuntimeError> {
    match value {
        Value::String(child_id) => Ok(child_id),
        Value::Null => Err(RuntimeError::RecipeLoad(format!(
            "{path}: expected node id string, got null"
        ))),
        Value::Array(_) => Err(RuntimeError::RecipeLoad(format!(
            "{path}: expected node id string, got array"
        ))),
        _ => Err(RuntimeError::RecipeLoad(format!(
            "{path}: expected node id string"
        ))),
    }
}

fn validate_input_name(name: &str, path: &str) -> Result<(), RuntimeError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: input name must not be empty"
        )));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: input name '{name}' must start with an ASCII letter or underscore"
        )));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: input name '{name}' must contain only ASCII letters, digits, and underscores"
        )));
    }
    Ok(())
}

fn take_string(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<String, RuntimeError> {
    let value = object.remove(field).ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: missing required field '{field}'"))
    })?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}.{field}: expected string")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collect_one(
        request: &Value,
    ) -> Result<(CollectedGraph, HashMap<BuildKey, PlannedNode>), RuntimeError> {
        let request = parse_request_value(request.clone(), "$")?;
        let mut nodes = HashMap::new();
        let graph = collect_graph(&request, "root", &mut nodes)?;
        Ok((graph, nodes))
    }

    fn tree_config(path: &str, text: &str, executable: bool) -> Value {
        json!({
            "tree": {
                "entries": [{
                    "type": "file",
                    "path": path,
                    "text": text,
                    "executable": executable
                }]
            }
        })
    }

    fn tree_recipe(name: &str, path: &str, text: &str, executable: bool) -> Value {
        json!({
            "name": name,
            "tag": "Tree",
            "config": tree_config(path, text, executable),
            "inputs": {}
        })
    }

    #[test]
    fn recipe_requires_top_level_root_node() {
        let error = parse_request_value(json!({"kind":"Legacy"}), "$").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing required top-level node 'root'"),
            "{error}"
        );
    }

    #[test]
    fn unknown_builder_tag_is_rejected() {
        let request = json!({
            "root": {
                "name": "broken",
                "tag": "NoSuchBuilder",
                "config": {},
                "inputs": {}
            }
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown builder tag 'NoSuchBuilder'"),
            "{error}"
        );
    }

    #[test]
    fn source_without_config_or_inputs_is_accepted() {
        let request = json!({
            "root": {
                "name": "local-source",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                "origin": {
                    "tag": "Path",
                    "path": "/tmp/source.tar",
                    "unpack": true
                }
            }
        });
        let (graph, nodes) = collect_one(&request).unwrap();
        let node = nodes.get(&graph.root_key).unwrap();
        assert!(matches!(node.recipe, PlannedRecipe::Source(_)));
    }

    #[test]
    fn source_with_origin_is_accepted() {
        let request = json!({
            "root": {
                "name": "local-source",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                "origin": {
                    "tag": "Path",
                    "path": "/tmp/source.tar",
                    "unpack": true
                }
            }
        });
        let (graph, nodes) = collect_one(&request).unwrap();
        let node = nodes.get(&graph.root_key).unwrap();
        assert!(matches!(node.recipe, PlannedRecipe::Source(_)));
    }

    #[test]
    fn source_without_origin_is_accepted() {
        let request = json!({
            "root": {
                "name": "local-source",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111"
            }
        });
        let (graph, nodes) = collect_one(&request).unwrap();
        let node = nodes.get(&graph.root_key).unwrap();
        let PlannedRecipe::Source(source) = &node.recipe else {
            panic!("expected source recipe");
        };
        assert!(source.origin.is_none());
    }

    #[test]
    fn source_oci_registry_origin_is_accepted() {
        let request = json!({
            "root": {
                "name": "base-image",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                "origin": {
                    "tag": "OciRegistry",
                    "image": "docker.io/library/alpine:3.20",
                    "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }
            }
        });
        let (graph, nodes) = collect_one(&request).unwrap();
        let node = nodes.get(&graph.root_key).unwrap();
        let PlannedRecipe::Source(source) = &node.recipe else {
            panic!("expected source recipe");
        };
        assert_eq!(source.origin.as_ref().unwrap().spec().tag, "OciRegistry");
    }

    #[test]
    fn source_path_origin_requires_absolute_paths() {
        let request = json!({
            "root": {
                "name": "local-source",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                "origin": {
                    "tag": "Path",
                    "path": "source.tar",
                    "unpack": true
                }
            }
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error.to_string().contains("expected absolute path"),
            "{error}"
        );
    }

    #[test]
    fn source_object_hash_allows_trailing_whitespace() {
        let request = json!({
            "root": {
                "name": "local-source",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111\n"
            }
        });
        let (graph, nodes) = collect_one(&request).unwrap();
        let node = nodes.get(&graph.root_key).unwrap();
        let PlannedRecipe::Source(source) = &node.recipe else {
            panic!("expected source recipe");
        };
        assert_eq!(
            source.object_hash.to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn extra_input_slot_is_rejected() {
        let request = json!({
            "root": {
                "name": "tree",
                "tag": "Tree",
                "config": tree_config("hello.txt", "hello", false),
                "inputs": { "unexpected": "dep" }
            },
            "dep": tree_recipe("dep", "dep.txt", "dep", false)
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not accept extra input 'unexpected'"),
            "{error}"
        );
    }

    #[test]
    fn missing_required_input_slot_is_rejected() {
        let request = json!({
            "root": {
                "name": "sandbox",
                "tag": "Sandbox",
                "config": {},
                "inputs": { "script": "script" }
            },
            "script": tree_recipe("script", "script.sh", "#!/bin/sh\n", true)
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("builder 'Sandbox' is missing required input 'rootfs'"),
            "{error}"
        );
    }

    #[test]
    fn non_string_input_is_rejected() {
        let request = json!({
            "root": {
                "name": "sandbox",
                "tag": "Sandbox",
                "config": {},
                "inputs": {
                    "rootfs": []
                }
            }
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected node id string, got array"),
            "{error}"
        );
    }

    #[test]
    fn build_key_order_follows_builder_spec_not_json_field_order() {
        let rootfs = json!({
            "name": "rootfs",
            "tag": "Tree",
            "config": tree_config("rootfs.txt", "rootfs", false),
            "inputs": {}
        });
        let script = json!({
            "name": "script",
            "tag": "Tree",
            "config": tree_config("script.sh", "#!/bin/sh\nexit 0\n", true),
            "inputs": {}
        });
        let source = json!({
            "name": "source",
            "tag": "Source",
            "object_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "origin": {
                "tag": "Http",
                "url": "https://example.invalid/source.tar.gz",
                "unpack": true
            }
        });
        let request = json!({
            "root": {
                "name": "sandbox",
                "tag": "Sandbox",
                "config": {},
                "inputs": {
                    "source": "source",
                    "rootfs": "rootfs",
                    "script": "script"
                }
            },
            "rootfs": rootfs.clone(),
            "script": script.clone(),
            "source": source.clone()
        });

        let (graph, _) = collect_one(&request).unwrap();
        let rootfs_key = compute_build_key("Tree", &rootfs["config"], &[]).unwrap();
        let script_key = compute_build_key("Tree", &script["config"], &[]).unwrap();
        let source_key = source_planning_key(compute_result_id(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let expected = compute_build_key(
            "Sandbox",
            &request["root"]["config"],
            &[rootfs_key, script_key, source_key],
        )
        .unwrap();

        assert_eq!(graph.root_key, expected);
    }

    #[test]
    fn collect_graph_keeps_first_publish_name_for_deduped_nodes() {
        let request = json!({
            "root": {
                "name": "final-group",
                "tag": "Group",
                "config": {},
                "inputs": {
                    "in001": "binary-b",
                    "in000": "binary-a"
                }
            },
            "binary-a": tree_recipe("binary-a", "same.txt", "same", false),
            "binary-b": tree_recipe("binary-b", "same.txt", "same", false)
        });

        let (graph, nodes) = collect_one(&request).unwrap();
        let a_key = *graph.node_keys.get("binary-a").unwrap();
        let b_key = *graph.node_keys.get("binary-b").unwrap();
        assert_eq!(a_key, b_key);
        let node = nodes.get(&a_key).unwrap();
        assert_eq!(node.publish_name, "binary-a");
        assert_eq!(graph.topo_order.last().map(String::as_str), Some("root"));
    }

    #[test]
    fn cycles_are_rejected() {
        let request = json!({
            "root": {
                "name": "a",
                "tag": "Sandbox",
                "config": {},
                "inputs": {
                    "rootfs": "root",
                    "script": "script"
                }
            },
            "script": tree_recipe("script", "script.sh", "#!/bin/sh\n", true)
        });

        let error = collect_one(&request).unwrap_err();
        assert!(error.to_string().contains("contains a cycle"), "{error}");
    }

    #[test]
    fn dangling_references_are_rejected() {
        let request = json!({
            "root": {
                "name": "sandbox",
                "tag": "Sandbox",
                "config": {},
                "inputs": {
                    "rootfs": "missing-rootfs",
                    "script": "script"
                }
            },
            "script": tree_recipe("script", "script.sh", "#!/bin/sh\n", true)
        });

        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown node id 'missing-rootfs'"),
            "{error}"
        );
    }

    #[test]
    fn old_nested_root_shape_is_rejected() {
        let old_shape = json!({
            "name": "hello",
            "tag": "Tree",
            "config": tree_config("hello.txt", "hi", false),
            "inputs": {}
        });

        let error = RecipeEnvelope::parse_json(serde_json::to_vec(&old_shape).unwrap().as_slice())
            .unwrap_err();
        assert!(
            error.to_string().contains("missing required field 'paths'"),
            "{error}"
        );
    }
}
