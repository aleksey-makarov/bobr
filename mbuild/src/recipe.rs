use crate::builders;
use crate::runtime::{RuntimeError, map_store_error};
use mbuild_core::{BuildKey, BuilderSpec, InputArity, compute_build_key};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone)]
pub struct RecipeRequest {
    nodes: BTreeMap<String, Recipe>,
}

#[derive(Debug, Clone)]
pub struct Recipe {
    name: String,
    tag: String,
    config: Value,
    inputs: BTreeMap<String, RecipeInputValue>,
}

#[derive(Debug, Clone)]
pub(crate) enum RecipeInputValue {
    One(String),
    Null,
    Many(Vec<String>),
}

impl RecipeRequest {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            RuntimeError::RecipeLoad(format!("failed to decode recipe JSON value: {error}"))
        })?;
        parse_request_value(value, "$")
    }

    pub(crate) fn node(&self, id: &str) -> Result<&Recipe, RuntimeError> {
        self.nodes.get(id).ok_or_else(|| {
            RuntimeError::InvalidRequest(format!("request references unknown node id '{id}'"))
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedRecipe {
    name: String,
    spec: &'static BuilderSpec,
    config: Value,
    inputs: BTreeMap<String, PlannedInputValue>,
}

impl PlannedRecipe {
    pub(crate) fn build_name(&self) -> &str {
        &self.name
    }

    pub(crate) fn builder_tag(&self) -> &'static str {
        self.spec.tag
    }

    pub(crate) fn config(&self) -> &Value {
        &self.config
    }

    pub(crate) fn inputs(&self) -> &BTreeMap<String, PlannedInputValue> {
        &self.inputs
    }

    pub(crate) fn try_for_each_direct_dep<E>(
        &self,
        mut f: impl FnMut(BuildKey) -> Result<(), E>,
    ) -> Result<(), E> {
        for slot in self.spec.inputs {
            let input = self
                .inputs
                .get(slot.name)
                .expect("planned recipe inputs must match builder spec");
            match input {
                PlannedInputValue::One(key) => f(*key)?,
                PlannedInputValue::Optional(Some(key)) => f(*key)?,
                PlannedInputValue::Optional(None) => {}
                PlannedInputValue::Many(keys) => {
                    for key in keys {
                        f(*key)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PlannedInputValue {
    One(BuildKey),
    Optional(Option<BuildKey>),
    Many(Vec<BuildKey>),
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedNode {
    pub(crate) recipe: PlannedRecipe,
    pub(crate) active_names: BTreeSet<String>,
    pub(crate) state: PlanningState,
}

impl PlannedNode {
    pub(crate) fn new(recipe: PlannedRecipe) -> Self {
        Self {
            recipe,
            active_names: BTreeSet::new(),
            state: PlanningState::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PlanningState {
    Unknown,
    Reused {
        build: mbuild_core::Build,
        origin: ReuseOrigin,
    },
    NeedsBuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReuseOrigin {
    BuildHandle,
    CanonicalResult,
}

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
    let builder = builders::get_builder(&recipe.tag).ok_or_else(|| {
        RuntimeError::UnknownBuilder(format!(
            "unknown builder tag '{}'; supported builders: {}",
            recipe.tag,
            builders::supported_builder_tags().join(", ")
        ))
    })?;
    let spec = builder.spec();

    for input_name in recipe.inputs.keys() {
        if spec.inputs.iter().all(|slot| slot.name != input_name) {
            return Err(RuntimeError::InvalidRequest(format!(
                "builder '{}' does not define input slot '{}'; allowed slots: {}",
                spec.tag,
                input_name,
                spec.inputs
                    .iter()
                    .map(|slot| slot.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }

    let mut inputs = BTreeMap::new();
    let mut ordered_direct_deps = Vec::new();
    for slot in spec.inputs {
        let input = recipe.inputs.get(slot.name).ok_or_else(|| {
            RuntimeError::InvalidRequest(format!(
                "builder '{}' is missing required input slot '{}' in recipe '{}'",
                spec.tag, slot.name, recipe.name
            ))
        })?;
        let planned = match (slot.arity, input) {
            (InputArity::One, RecipeInputValue::One(child_id)) => {
                let key =
                    collect_graph_inner(request, child_id, nodes, stack, node_keys, topo_order)?;
                ordered_direct_deps.push(key);
                PlannedInputValue::One(key)
            }
            (InputArity::Optional, RecipeInputValue::Null) => PlannedInputValue::Optional(None),
            (InputArity::Optional, RecipeInputValue::One(child_id)) => {
                let key =
                    collect_graph_inner(request, child_id, nodes, stack, node_keys, topo_order)?;
                ordered_direct_deps.push(key);
                PlannedInputValue::Optional(Some(key))
            }
            (InputArity::Many, RecipeInputValue::Many(child_ids)) => {
                let mut keys = Vec::with_capacity(child_ids.len());
                for child_id in child_ids {
                    let key = collect_graph_inner(
                        request, child_id, nodes, stack, node_keys, topo_order,
                    )?;
                    ordered_direct_deps.push(key);
                    keys.push(key);
                }
                PlannedInputValue::Many(keys)
            }
            (InputArity::One, _) => {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' input slot '{}' must be a single node id string",
                    spec.tag, slot.name
                )));
            }
            (InputArity::Optional, _) => {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' optional input slot '{}' must be null or a single node id string",
                    spec.tag, slot.name
                )));
            }
            (InputArity::Many, _) => {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' repeated input slot '{}' must be an array of node id strings",
                    spec.tag, slot.name
                )));
            }
        };
        inputs.insert(slot.name.to_string(), planned);
    }

    stack.remove(node_id);

    let key = compute_build_key(spec.tag, &recipe.config, &ordered_direct_deps)
        .map_err(map_store_error)?;
    nodes.entry(key).or_insert_with(|| {
        PlannedNode::new(PlannedRecipe {
            name: recipe.name.clone(),
            spec,
            config: recipe.config.clone(),
            inputs: inputs.clone(),
        })
    });
    if let Some(node) = nodes.get_mut(&key) {
        node.active_names.insert(recipe.name.clone());
    }
    node_keys.insert(node_id.to_string(), key);
    topo_order.push(node_id.to_string());
    Ok(key)
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
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}: expected recipe object")))?;

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
        let input_path = format!("{path}.inputs.{slot_name}");
        inputs.insert(slot_name, parse_input_value(slot_value, &input_path)?);
    }

    Ok(Recipe {
        name,
        tag,
        config,
        inputs,
    })
}

fn parse_input_value(value: Value, path: &str) -> Result<RecipeInputValue, RuntimeError> {
    match value {
        Value::Null => Ok(RecipeInputValue::Null),
        Value::Array(items) => {
            let mut children = Vec::with_capacity(items.len());
            for (index, item) in items.into_iter().enumerate() {
                let child_id = item.as_str().ok_or_else(|| {
                    RuntimeError::RecipeLoad(format!("{path}[{index}]: expected node id string"))
                })?;
                children.push(child_id.to_string());
            }
            Ok(RecipeInputValue::Many(children))
        }
        Value::String(child_id) => Ok(RecipeInputValue::One(child_id)),
        _ => Err(RuntimeError::RecipeLoad(format!(
            "{path}: expected null, node id string, or array of node id strings"
        ))),
    }
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
    use mbuild_core::compute_build_key;
    use serde_json::json;

    fn collect_one(
        request: &Value,
    ) -> Result<(CollectedGraph, HashMap<BuildKey, PlannedNode>), RuntimeError> {
        let request = parse_request_value(request.clone(), "$")?;
        let mut nodes = HashMap::new();
        let graph = collect_graph(&request, "root", &mut nodes)?;
        Ok((graph, nodes))
    }

    #[test]
    fn recipe_requires_top_level_root_node() {
        let error = RecipeRequest::parse_json(br#"{"kind":"Text"}"#).unwrap_err();
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
    fn extra_input_slot_is_rejected() {
        let request = json!({
            "root": {
                "name": "text",
                "tag": "Text",
                "config": { "source": "hello", "executable": false },
                "inputs": { "unexpected": null }
            }
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not define input slot 'unexpected'"),
            "{error}"
        );
    }

    #[test]
    fn missing_required_input_slot_is_rejected() {
        let request = json!({
            "root": {
                "name": "img",
                "tag": "Image",
                "config": { "mode": "bootstrap" },
                "inputs": {}
            }
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing required input slot 'base'"),
            "{error}"
        );
    }

    #[test]
    fn wrong_input_arity_is_rejected() {
        let request = json!({
            "root": {
                "name": "bin",
                "tag": "Binary",
                "config": {},
                "inputs": {
                    "image": [],
                    "in": []
                }
            }
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("input slot 'image' must be a single node id string"),
            "{error}"
        );
    }

    #[test]
    fn old_binary_input_shape_is_rejected() {
        let request = json!({
            "root": {
                "name": "bin",
                "tag": "Binary",
                "config": {},
                "inputs": {
                    "image": "image",
                    "script": "script",
                    "sources": []
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
                "config": { "source": "#!/bin/sh\n", "executable": true },
                "inputs": {}
            }
        });
        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not define input slot 'script'"),
            "{error}"
        );
    }

    #[test]
    fn build_key_order_follows_builder_spec_not_json_field_order() {
        let image = json!({
            "name": "base-image",
            "tag": "ContainerImage",
            "config": {
                "image": "docker.io/library/buildpack-deps:bookworm",
                "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            },
            "inputs": {}
        });
        let script = json!({
            "name": "script",
            "tag": "Text",
            "config": {
                "source": "#!/bin/sh\nexit 0\n",
                "executable": true
            },
            "inputs": {}
        });
        let source = json!({
            "name": "source",
            "tag": "Fetch",
            "config": {
                "url": "https://example.invalid/source.tar.gz",
                "hash": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "unpack": true
            },
            "inputs": {}
        });
        let request = json!({
            "root": {
                "name": "bin",
                "tag": "Binary",
                "config": {},
                "inputs": {
                    "in": ["script", "source"],
                    "image": "image"
                }
            },
            "image": image.clone(),
            "script": script.clone(),
            "source": source.clone()
        });

        let (graph, _) = collect_one(&request).unwrap();
        let image_key = compute_build_key("ContainerImage", &image["config"], &[]).unwrap();
        let script_key = compute_build_key("Text", &script["config"], &[]).unwrap();
        let source_key = compute_build_key("Fetch", &source["config"], &[]).unwrap();
        let expected = compute_build_key(
            "Binary",
            &request["root"]["config"],
            &[image_key, script_key, source_key],
        )
        .unwrap();

        assert_eq!(graph.root_key, expected);
    }

    #[test]
    fn collect_graph_populates_active_names_and_node_keys() {
        let request = json!({
            "root": {
                "name": "final-image",
                "tag": "Image",
                "config": { "mode": "bootstrap" },
                "inputs": {
                    "base": null,
                    "inputs": ["binary-a", "binary-b"]
                }
            },
            "binary-a": {
                "name": "binary-a",
                "tag": "Text",
                "config": { "source": "same", "executable": false },
                "inputs": {}
            },
            "binary-b": {
                "name": "binary-b",
                "tag": "Text",
                "config": { "source": "same", "executable": false },
                "inputs": {}
            }
        });

        let (graph, nodes) = collect_one(&request).unwrap();
        let a_key = *graph.node_keys.get("binary-a").unwrap();
        let b_key = *graph.node_keys.get("binary-b").unwrap();
        assert_eq!(a_key, b_key);
        let node = nodes.get(&a_key).unwrap();
        assert!(node.active_names.contains("binary-a"));
        assert!(node.active_names.contains("binary-b"));
        assert_eq!(graph.topo_order.last().map(String::as_str), Some("root"));
    }

    #[test]
    fn cycles_are_rejected() {
        let request = json!({
            "root": {
                "name": "a",
                "tag": "Binary",
                "config": {},
                "inputs": {
                    "image": "root",
                    "in": ["script"]
                }
            },
            "script": {
                "name": "script",
                "tag": "Text",
                "config": { "source": "#!/bin/sh\n", "executable": true },
                "inputs": {}
            }
        });

        let error = collect_one(&request).unwrap_err();
        assert!(error.to_string().contains("contains a cycle"), "{error}");
    }

    #[test]
    fn dangling_references_are_rejected() {
        let request = json!({
            "root": {
                "name": "bin",
                "tag": "Binary",
                "config": {},
                "inputs": {
                    "image": "missing-image",
                    "in": ["script"]
                }
            },
            "script": {
                "name": "script",
                "tag": "Text",
                "config": { "source": "#!/bin/sh\n", "executable": true },
                "inputs": {}
            }
        });

        let error = collect_one(&request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown node id 'missing-image'"),
            "{error}"
        );
    }

    #[test]
    fn old_nested_root_shape_is_rejected() {
        let old_shape = json!({
            "name": "hello",
            "tag": "Text",
            "config": { "source": "hi", "executable": false },
            "inputs": {}
        });

        let error = RecipeRequest::parse_json(serde_json::to_vec(&old_shape).unwrap().as_slice())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing required top-level node 'root'"),
            "{error}"
        );
    }
}
