use crate::builders;
use crate::runtime::{RuntimeError, map_store_error};
use mbuild_core::{BuildKey, BuilderSpec, InputArity, compute_build_key};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone)]
pub struct Recipe {
    name: String,
    tag: String,
    config: Value,
    inputs: BTreeMap<String, RecipeInputValue>,
}

#[derive(Debug, Clone)]
pub(crate) enum RecipeInputValue {
    Node(Box<Recipe>),
    Null,
    Many(Vec<Recipe>),
}

impl Recipe {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            RuntimeError::RecipeLoad(format!("failed to decode recipe JSON value: {error}"))
        })?;
        parse_recipe_value(value, "$")
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn direct_children(&self) -> Vec<&Recipe> {
        let mut children = Vec::new();
        for input in self.inputs.values() {
            match input {
                RecipeInputValue::Node(child) => children.push(child.as_ref()),
                RecipeInputValue::Null => {}
                RecipeInputValue::Many(many) => children.extend(many.iter()),
            }
        }
        children
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

pub(crate) fn collect_graph(
    recipe: &Recipe,
    nodes: &mut HashMap<BuildKey, PlannedNode>,
) -> Result<BuildKey, RuntimeError> {
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
            (InputArity::One, RecipeInputValue::Node(child)) => {
                let key = collect_graph(child, nodes)?;
                ordered_direct_deps.push(key);
                PlannedInputValue::One(key)
            }
            (InputArity::Optional, RecipeInputValue::Null) => PlannedInputValue::Optional(None),
            (InputArity::Optional, RecipeInputValue::Node(child)) => {
                let key = collect_graph(child, nodes)?;
                ordered_direct_deps.push(key);
                PlannedInputValue::Optional(Some(key))
            }
            (InputArity::Many, RecipeInputValue::Many(children)) => {
                let mut keys = Vec::with_capacity(children.len());
                for child in children {
                    let key = collect_graph(child, nodes)?;
                    ordered_direct_deps.push(key);
                    keys.push(key);
                }
                PlannedInputValue::Many(keys)
            }
            (InputArity::One, _) => {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' input slot '{}' must be a single recipe object",
                    spec.tag, slot.name
                )));
            }
            (InputArity::Optional, _) => {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' optional input slot '{}' must be null or a single recipe object",
                    spec.tag, slot.name
                )));
            }
            (InputArity::Many, _) => {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' repeated input slot '{}' must be an array of recipe objects",
                    spec.tag, slot.name
                )));
            }
        };
        inputs.insert(slot.name.to_string(), planned);
    }

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
    Ok(key)
}

fn parse_recipe_value(value: Value, path: &str) -> Result<Recipe, RuntimeError> {
    let mut object = value.as_object().cloned().ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: expected recipe object"))
    })?;

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

    let inputs_object = inputs_value.as_object().cloned().ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}.inputs: expected object"))
    })?;
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
                children.push(parse_recipe_value(item, &format!("{path}[{index}]"))?);
            }
            Ok(RecipeInputValue::Many(children))
        }
        Value::Object(_) => Ok(RecipeInputValue::Node(Box::new(parse_recipe_value(
            value, path,
        )?))),
        _ => Err(RuntimeError::RecipeLoad(format!(
            "{path}: expected null, recipe object, or array of recipe objects"
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
    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}.{field}: expected string"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::compute_build_key;
    use serde_json::json;

    fn collect_one(recipe: &Value) -> Result<(BuildKey, HashMap<BuildKey, PlannedNode>), RuntimeError> {
        let recipe = parse_recipe_value(recipe.clone(), "$")?;
        let mut nodes = HashMap::new();
        let key = collect_graph(&recipe, &mut nodes)?;
        Ok((key, nodes))
    }

    #[test]
    fn recipe_requires_generic_shape() {
        let error = Recipe::parse_json(br#"{"kind":"Text"}"#).unwrap_err();
        assert!(error.to_string().contains("missing required field 'name'"), "{error}");
    }

    #[test]
    fn unknown_builder_tag_is_rejected() {
        let recipe = json!({
            "name": "broken",
            "tag": "NoSuchBuilder",
            "config": {},
            "inputs": {}
        });
        let error = collect_one(&recipe).unwrap_err();
        assert!(error.to_string().contains("unknown builder tag 'NoSuchBuilder'"), "{error}");
    }

    #[test]
    fn extra_input_slot_is_rejected() {
        let recipe = json!({
            "name": "text",
            "tag": "Text",
            "config": { "kind": "plain-text", "source": "hello" },
            "inputs": { "unexpected": null }
        });
        let error = collect_one(&recipe).unwrap_err();
        assert!(error.to_string().contains("does not define input slot 'unexpected'"), "{error}");
    }

    #[test]
    fn missing_required_input_slot_is_rejected() {
        let recipe = json!({
            "name": "img",
            "tag": "Image",
            "config": { "mode": "bootstrap" },
            "inputs": {}
        });
        let error = collect_one(&recipe).unwrap_err();
        assert!(error.to_string().contains("missing required input slot 'base'"), "{error}");
    }

    #[test]
    fn wrong_input_arity_is_rejected() {
        let recipe = json!({
            "name": "bin",
            "tag": "Binary",
            "config": { "kind": "binary-output" },
            "inputs": {
                "image": [],
                "script": null,
                "sources": []
            }
        });
        let error = collect_one(&recipe).unwrap_err();
        assert!(error.to_string().contains("input slot 'image' must be a single recipe object"), "{error}");
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
                "kind": "build-script",
                "source": "#!/bin/sh\nexit 0\n"
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
        let recipe = json!({
            "name": "bin",
            "tag": "Binary",
            "config": { "kind": "binary-output", "optimize": "size" },
            "inputs": {
                "sources": [source.clone()],
                "script": script.clone(),
                "image": image.clone()
            }
        });

        let (key, _) = collect_one(&recipe).unwrap();
        let image_key = compute_build_key("ContainerImage", &image["config"], &[]).unwrap();
        let script_key = compute_build_key("Text", &script["config"], &[]).unwrap();
        let source_key = compute_build_key("Fetch", &source["config"], &[]).unwrap();
        let expected = compute_build_key(
            "Binary",
            &recipe["config"],
            &[image_key, script_key, source_key],
        )
        .unwrap();

        assert_eq!(key, expected);
    }
}
