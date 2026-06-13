use crate::builders;
use crate::planned::PlannedSubject;
use crate::runtime::RuntimeError;
use mbuild_core::BuildKey;
#[cfg(test)]
use mbuild_core::compute_build_key;
use mbuild_source::parse_source_subject;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct RecipeEnvelope {
    pub options: RecipeOptions,
    pub request: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct RecipeOptions {
    pub store: Option<PathBuf>,
    pub quiet: Option<bool>,
    pub jobs: Option<usize>,
}

impl RecipeEnvelope {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            RuntimeError::RecipeLoad(format!("failed to decode recipe JSON value: {error}"))
        })?;
        parse_envelope_value(value, "$")
    }
}

pub(crate) fn collect_graph(
    request: &BTreeMap<String, Value>,
    subjects: &mut HashMap<BuildKey, Arc<PlannedSubject>>,
) -> Result<BuildKey, RuntimeError> {
    let mut visited_in_path = BTreeSet::new();
    let mut node_keys = HashMap::new();
    collect_graph_inner(
        request,
        "root",
        subjects,
        &mut visited_in_path,
        &mut node_keys,
    )
}

fn collect_graph_inner(
    request: &BTreeMap<String, Value>,
    node_id: &str,
    subjects: &mut HashMap<BuildKey, Arc<PlannedSubject>>,
    visited_in_path: &mut BTreeSet<String>,
    node_keys: &mut HashMap<String, BuildKey>,
) -> Result<BuildKey, RuntimeError> {
    if let Some(existing) = node_keys.get(node_id) {
        return Ok(*existing);
    }

    if !visited_in_path.insert(node_id.to_string()) {
        return Err(RuntimeError::InvalidRequest(format!(
            "request graph contains a cycle through node id '{node_id}'"
        )));
    }

    let node_value = request.get(node_id).ok_or_else(|| {
        RuntimeError::InvalidRequest(format!("request references unknown node id '{node_id}'"))
    })?;
    let node_path = node_path(node_id);
    let mut object = node_value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{node_path}: expected recipe object")))?;
    let tag = take_string(&mut object, &node_path, "tag")?;

    let (key, subject) = if tag == "Source" {
        let subject = parse_source_subject(object, &node_path)
            .map_err(|error| RuntimeError::RecipeLoad(error.to_string()))?;
        let key = subject.build_key();
        (key, Arc::new(PlannedSubject::Source(subject)))
    } else {
        let inputs_value = object.remove("inputs").ok_or_else(|| {
            RuntimeError::RecipeLoad(format!("{node_path}: missing required field 'inputs'"))
        })?;

        let inputs_object = inputs_value.as_object().cloned().ok_or_else(|| {
            RuntimeError::RecipeLoad(format!("{node_path}.inputs: expected object"))
        })?;
        let mut inputs = BTreeMap::new();
        for (input_name, slot_value) in inputs_object {
            validate_input_name(&input_name, &format!("{node_path}.inputs"))?;
            let input_path = format!("{node_path}.inputs.{input_name}");
            let child_id = parse_input_value(slot_value, &input_path)?;
            let child =
                collect_graph_inner(request, &child_id, subjects, visited_in_path, node_keys)?;
            inputs.insert(input_name, child);
        }

        let builder_subject = builders::parse_builder_subject(&tag, object, inputs, &node_path)?;
        (
            builder_subject.build_key(),
            Arc::new(PlannedSubject::Builder(builder_subject)),
        )
    };

    visited_in_path.remove(node_id);

    subjects.entry(key).or_insert_with(|| subject.clone());
    node_keys.insert(node_id.to_string(), key);
    Ok(key)
}

fn parse_envelope_value(value: Value, path: &str) -> Result<RecipeEnvelope, RuntimeError> {
    let mut object = value.as_object().cloned().ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: expected top-level recipe object"))
    })?;

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

    Ok(RecipeEnvelope { options, request })
}

fn parse_options_value(value: Value, path: &str) -> Result<RecipeOptions, RuntimeError> {
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}: expected object")))?;

    let store = match object.remove("store") {
        Some(Value::String(value)) => Some(PathBuf::from(value)),
        Some(_) => {
            return Err(RuntimeError::RecipeLoad(format!(
                "{path}.store: expected string"
            )));
        }
        None => None,
    };
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
    Ok(RecipeOptions { store, quiet, jobs })
}

fn parse_request_value(value: Value, path: &str) -> Result<BTreeMap<String, Value>, RuntimeError> {
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

    Ok(nodes)
}

fn parse_recipe_value(value: Value, path: &str) -> Result<Value, RuntimeError> {
    value
        .as_object()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}: expected recipe object")))?;
    Ok(value)
}

fn node_path(node_id: &str) -> String {
    format!("$.nodes.{node_id}")
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
    ) -> Result<(BuildKey, HashMap<BuildKey, Arc<PlannedSubject>>), RuntimeError> {
        let request = parse_request_value(request.clone(), "$")?;
        let mut subjects = HashMap::new();
        let root_key = collect_graph(&request, &mut subjects)?;
        Ok((root_key, subjects))
    }

    fn collect_error(request: &Value) -> RuntimeError {
        match collect_one(request) {
            Ok(_) => panic!("expected collect_graph to fail"),
            Err(error) => error,
        }
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
        let error = collect_error(&request);
        assert!(
            error
                .to_string()
                .contains("unknown builder tag 'NoSuchBuilder'"),
            "{error}"
        );
    }

    #[test]
    fn unreachable_node_recipe_fields_are_not_interpreted() {
        let request = json!({
            "root": tree_recipe("root", "hello.txt", "hello", false),
            "unused": {
                "tag": [],
                "name": 42,
                "config": "not checked"
            }
        });

        let (root_key, subjects) = collect_one(&request).unwrap();
        assert!(subjects.contains_key(&root_key));
        assert_eq!(subjects.len(), 1);
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
        let (root_key, subjects) = collect_one(&request).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "local-source");
        assert_eq!(subject.tag(), "Source");
        assert!(matches!(subject.as_ref(), PlannedSubject::Source(_)));
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
        let (root_key, subjects) = collect_one(&request).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "local-source");
        assert_eq!(subject.tag(), "Source");
        assert!(matches!(subject.as_ref(), PlannedSubject::Source(_)));
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
        let (root_key, subjects) = collect_one(&request).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "local-source");
        assert_eq!(subject.tag(), "Source");
        assert!(matches!(subject.as_ref(), PlannedSubject::Source(_)));
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
        let (root_key, subjects) = collect_one(&request).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "base-image");
        assert_eq!(subject.tag(), "Source");
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
        let error = collect_error(&request);
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
        let (root_key, subjects) = collect_one(&request).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        let object_hash = "1111111111111111111111111111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let expected_key = BuildKey::from_object_hash(object_hash);
        assert_eq!(root_key, expected_key);
        assert!(
            matches!(subject.as_ref(), PlannedSubject::Source(source) if source.declared_object_hash() == object_hash)
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
        let error = collect_error(&request);
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
        let error = collect_error(&request);
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
        let error = collect_error(&request);
        assert!(
            error
                .to_string()
                .contains("expected node id string, got array"),
            "{error}"
        );
    }

    #[test]
    fn build_key_order_follows_input_spec_not_json_field_order() {
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

        let (root_key, _) = collect_one(&request).unwrap();
        let rootfs_key = compute_build_key("Tree", &rootfs["config"], &[]).unwrap();
        let script_key = compute_build_key("Tree", &script["config"], &[]).unwrap();
        let source_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .parse()
            .unwrap();
        let source_key = BuildKey::from_object_hash(source_hash);
        let expected = compute_build_key(
            "Sandbox",
            &request["root"]["config"],
            &[rootfs_key, script_key, source_key],
        )
        .unwrap();

        assert_eq!(root_key, expected);
    }

    #[test]
    fn collect_graph_keeps_first_representative_recipe_for_deduped_nodes() {
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

        let (_root_key, subjects) = collect_one(&request).unwrap();
        let deduped_key =
            compute_build_key("Tree", &tree_config("same.txt", "same", false), &[]).unwrap();
        let subject = subjects.get(&deduped_key).unwrap();
        assert_eq!(subject.name(), "binary-a");
        assert_eq!(subject.tag(), "Tree");
        assert!(
            matches!(subject.as_ref(), PlannedSubject::Builder(builder) if builder.build_key() == deduped_key)
        );
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

        let error = collect_error(&request);
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

        let error = collect_error(&request);
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
            error.to_string().contains("missing required field 'nodes'"),
            "{error}"
        );
    }
}
