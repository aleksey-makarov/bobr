use crate::execution::ExecutionError;
use crate::planned::PlannedSubject;
use bobr_builder::BuilderPlanError;
use bobr_core::BuildKey;
#[cfg(test)]
use bobr_core::{ConfigDigest, compute_build_key};
use bobr_source::parse_source_subject;
use bobr_store::validate_ref_name;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

pub(crate) fn collect_graph(
    nodes: &BTreeMap<String, Value>,
    subjects: &mut HashMap<BuildKey, Arc<PlannedSubject>>,
) -> Result<BuildKey, ExecutionError> {
    let mut visited_in_path = BTreeSet::new();
    let mut node_keys = HashMap::new();
    collect_graph_inner(
        nodes,
        "root",
        subjects,
        &mut visited_in_path,
        &mut node_keys,
    )
}

fn collect_graph_inner(
    nodes: &BTreeMap<String, Value>,
    node_id: &str,
    subjects: &mut HashMap<BuildKey, Arc<PlannedSubject>>,
    visited_in_path: &mut BTreeSet<String>,
    node_keys: &mut HashMap<String, BuildKey>,
) -> Result<BuildKey, ExecutionError> {
    if let Some(existing) = node_keys.get(node_id) {
        return Ok(*existing);
    }

    if !visited_in_path.insert(node_id.to_string()) {
        return Err(ExecutionError::InvalidRequest(format!(
            "request graph contains a cycle through node id '{node_id}'"
        )));
    }

    let node_value = nodes.get(node_id).ok_or_else(|| {
        ExecutionError::InvalidRequest(format!("request references unknown node id '{node_id}'"))
    })?;
    let node_path = node_path(node_id);
    let mut object = node_value.as_object().cloned().ok_or_else(|| {
        ExecutionError::RequestLoad(format!("{node_path}: expected request object"))
    })?;
    let tag = take_string(&mut object, &node_path, "tag")?;

    let (key, subject) = if tag == "Source" {
        let subject = parse_source_subject(object)
            .map_err(|error| ExecutionError::RequestLoad(format!("{node_path}: {error}")))?;
        let key = subject.build_key();
        (key, Arc::new(PlannedSubject::Source(subject)))
    } else {
        let inputs_value = object.remove("inputs").ok_or_else(|| {
            ExecutionError::RequestLoad(format!("{node_path}: missing required field 'inputs'"))
        })?;

        let inputs_object = inputs_value.as_object().cloned().ok_or_else(|| {
            ExecutionError::RequestLoad(format!("{node_path}.inputs: expected object"))
        })?;
        let mut inputs = BTreeMap::new();
        for (input_name, slot_value) in inputs_object {
            let input_path = format!("{node_path}.inputs.{input_name}");
            let child_id = parse_input_value(slot_value, &input_path)?;
            let child =
                collect_graph_inner(nodes, &child_id, subjects, visited_in_path, node_keys)?;
            inputs.insert(input_name, child);
        }

        let builder_subject = crate::builder_registry::parse_subject(&tag, object, inputs)
            .map_err(|error| map_builder_plan_error(error, &node_path))?;
        (
            builder_subject.build_key(),
            Arc::new(PlannedSubject::Builder(builder_subject)),
        )
    };

    // Validate the recipe node name early (it later becomes a store ref name),
    // so a bad name fails during planning rather than after the node is built.
    validate_ref_name(subject.name())
        .map_err(|error| ExecutionError::RequestLoad(format!("{node_path}: {error}")))?;

    visited_in_path.remove(node_id);

    subjects.entry(key).or_insert_with(|| subject.clone());
    node_keys.insert(node_id.to_string(), key);
    Ok(key)
}

fn map_builder_plan_error(error: BuilderPlanError, node_path: &str) -> ExecutionError {
    let message = format!("{node_path}: {error}");
    match error {
        BuilderPlanError::UnknownBuilder { .. } => ExecutionError::UnknownBuilder(message),
        BuilderPlanError::Recipe(_) => ExecutionError::RequestLoad(message),
        BuilderPlanError::InvalidRequest(_) | BuilderPlanError::Identity(_) => {
            ExecutionError::InvalidRequest(message)
        }
    }
}

fn node_path(node_id: &str) -> String {
    format!("$.nodes.{node_id}")
}

fn parse_input_value(value: Value, path: &str) -> Result<String, ExecutionError> {
    match value {
        Value::String(child_id) => Ok(child_id),
        Value::Null => Err(ExecutionError::RequestLoad(format!(
            "{path}: expected node id string, got null"
        ))),
        Value::Array(_) => Err(ExecutionError::RequestLoad(format!(
            "{path}: expected node id string, got array"
        ))),
        _ => Err(ExecutionError::RequestLoad(format!(
            "{path}: expected node id string"
        ))),
    }
}

fn take_string(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<String, ExecutionError> {
    let value = object.remove(field).ok_or_else(|| {
        ExecutionError::RequestLoad(format!("{path}: missing required field '{field}'"))
    })?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ExecutionError::RequestLoad(format!("{path}.{field}: expected string")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::parse_request_nodes;
    use serde_json::json;

    fn collect_one(
        nodes: &Value,
    ) -> Result<(BuildKey, HashMap<BuildKey, Arc<PlannedSubject>>), ExecutionError> {
        let nodes = parse_request_nodes(nodes.clone(), "$")?;
        let mut subjects = HashMap::new();
        let root_key = collect_graph(&nodes, &mut subjects)?;
        Ok((root_key, subjects))
    }

    fn collect_error(nodes: &Value) -> ExecutionError {
        match collect_one(nodes) {
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
    fn unknown_builder_tag_is_rejected() {
        let nodes = json!({
            "root": {
                "name": "broken",
                "tag": "NoSuchBuilder",
                "config": {},
                "inputs": {}
            }
        });
        let error = collect_error(&nodes);
        assert!(
            error
                .to_string()
                .contains("unknown builder tag 'NoSuchBuilder'"),
            "{error}"
        );
    }

    #[test]
    fn invalid_recipe_node_name_is_rejected_during_planning() {
        let nodes = json!({
            "root": tree_recipe("bad/name", "hello.txt", "hello", false),
        });
        let error = collect_error(&nodes);
        assert!(
            error.to_string().contains("invalid ref name 'bad/name'"),
            "{error}"
        );
    }

    #[test]
    fn unreachable_node_recipe_fields_are_not_interpreted() {
        let nodes = json!({
            "root": tree_recipe("root", "hello.txt", "hello", false),
            "unused": {
                "tag": [],
                "name": 42,
                "config": "not checked"
            }
        });

        let (root_key, subjects) = collect_one(&nodes).unwrap();
        assert!(subjects.contains_key(&root_key));
        assert_eq!(subjects.len(), 1);
    }

    #[test]
    fn source_without_config_or_inputs_is_accepted() {
        let nodes = json!({
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
        let (root_key, subjects) = collect_one(&nodes).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "local-source");
        assert!(matches!(subject.as_ref(), PlannedSubject::Source(_)));
    }

    #[test]
    fn source_with_origin_is_accepted() {
        let nodes = json!({
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
        let (root_key, subjects) = collect_one(&nodes).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "local-source");
        assert!(matches!(subject.as_ref(), PlannedSubject::Source(_)));
    }

    #[test]
    fn source_without_origin_is_accepted() {
        let nodes = json!({
            "root": {
                "name": "local-source",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111"
            }
        });
        let (root_key, subjects) = collect_one(&nodes).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "local-source");
        assert!(matches!(subject.as_ref(), PlannedSubject::Source(_)));
    }

    #[test]
    fn source_oci_registry_origin_is_accepted() {
        let nodes = json!({
            "root": {
                "name": "base-image",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                "origin": {
                    "tag": "OciRegistry",
                    "image": "docker.io/library/alpine:3.20",
                    "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "platform": {
                        "os": "linux",
                        "architecture": "amd64"
                    }
                }
            }
        });
        let (root_key, subjects) = collect_one(&nodes).unwrap();
        let subject = subjects.get(&root_key).unwrap();
        assert_eq!(subject.name(), "base-image");
        assert!(matches!(subject.as_ref(), PlannedSubject::Source(_)));
    }

    #[test]
    fn source_path_origin_requires_absolute_paths() {
        let nodes = json!({
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
        let error = collect_error(&nodes);
        assert!(
            error.to_string().contains("expected absolute path"),
            "{error}"
        );
    }

    #[test]
    fn source_object_hash_allows_trailing_whitespace() {
        let nodes = json!({
            "root": {
                "name": "local-source",
                "tag": "Source",
                "object_hash": "1111111111111111111111111111111111111111111111111111111111111111\n"
            }
        });
        let (root_key, subjects) = collect_one(&nodes).unwrap();
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
        let nodes = json!({
            "root": {
                "name": "tree",
                "tag": "Tree",
                "config": tree_config("hello.txt", "hello", false),
                "inputs": { "unexpected": "dep" }
            },
            "dep": tree_recipe("dep", "dep.txt", "dep", false)
        });
        let error = collect_error(&nodes);
        assert!(
            error
                .to_string()
                .contains("does not accept extra input 'unexpected'"),
            "{error}"
        );
    }

    #[test]
    fn missing_required_input_slot_is_rejected() {
        let nodes = json!({
            "root": {
                "name": "sandbox",
                "tag": "Sandbox",
                "config": {},
                "inputs": { "script": "script" }
            },
            "script": tree_recipe("script", "script.sh", "#!/bin/sh\n", true)
        });
        let error = collect_error(&nodes);
        assert!(
            error
                .to_string()
                .contains("builder 'Sandbox' is missing required input '_rootfs'"),
            "{error}"
        );
    }

    #[test]
    fn non_string_input_is_rejected() {
        let nodes = json!({
            "root": {
                "name": "sandbox",
                "tag": "Sandbox",
                "config": {},
                "inputs": {
                    "rootfs": []
                }
            }
        });
        let error = collect_error(&nodes);
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
        let nodes = json!({
            "root": {
                "name": "sandbox",
                "tag": "Sandbox",
                // Canonical (normalized) Sandbox config, so the key computed
                // from it below matches the planner's normalized key.
                "config": { "script_config": {}, "steps": [], "preserve_ownership": true },
                "inputs": {
                    "source": "source",
                    "_rootfs": "rootfs",
                    "script": "script"
                }
            },
            "rootfs": rootfs.clone(),
            "script": script.clone(),
            "source": source.clone()
        });

        let (root_key, _) = collect_one(&nodes).unwrap();
        let tree_token = format!("{}/1", bobr_core::BOBR_BUILD_CORE_VERSION);
        let rootfs_key = compute_build_key(
            "Tree",
            &tree_token,
            ConfigDigest::of(&rootfs["config"]).unwrap(),
            &BTreeMap::new(),
        )
        .unwrap();
        let script_key = compute_build_key(
            "Tree",
            &tree_token,
            ConfigDigest::of(&script["config"]).unwrap(),
            &BTreeMap::new(),
        )
        .unwrap();
        let source_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .parse()
            .unwrap();
        let source_key = BuildKey::from_object_hash(source_hash);
        let expected = compute_build_key(
            "Sandbox",
            &format!(
                "{}/3@{}",
                bobr_core::BOBR_BUILD_CORE_VERSION,
                std::env::consts::ARCH
            ),
            ConfigDigest::of(&nodes["root"]["config"]).unwrap(),
            &BTreeMap::from([
                ("_rootfs".to_string(), rootfs_key),
                ("script".to_string(), script_key),
                ("source".to_string(), source_key),
            ]),
        )
        .unwrap();

        assert_eq!(root_key, expected);
    }

    #[test]
    fn collect_graph_keeps_first_representative_recipe_for_deduped_nodes() {
        let nodes = json!({
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

        let (_root_key, subjects) = collect_one(&nodes).unwrap();
        let deduped_key = compute_build_key(
            "Tree",
            &format!("{}/1", bobr_core::BOBR_BUILD_CORE_VERSION),
            ConfigDigest::of(&tree_config("same.txt", "same", false)).unwrap(),
            &BTreeMap::new(),
        )
        .unwrap();
        let subject = subjects.get(&deduped_key).unwrap();
        assert_eq!(subject.name(), "binary-a");
        assert!(
            matches!(subject.as_ref(), PlannedSubject::Builder(builder) if builder.build_key() == deduped_key)
        );
    }

    #[test]
    fn cycles_are_rejected() {
        let nodes = json!({
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

        let error = collect_error(&nodes);
        assert!(error.to_string().contains("contains a cycle"), "{error}");
    }

    #[test]
    fn dangling_references_are_rejected() {
        let nodes = json!({
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

        let error = collect_error(&nodes);
        assert!(
            error
                .to_string()
                .contains("unknown node id 'missing-rootfs'"),
            "{error}"
        );
    }
}
