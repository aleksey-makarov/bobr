use crate::execution::ExecutionError;
use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Schema marker for the request format. It deserializes only from the exact
/// schema string, so the format version is enforced declaratively at parse
/// time and never needs to live as data on `Request`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestSchemaV1;

impl<'de> Deserialize<'de> for RequestSchemaV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "bobr-request-v1" => Ok(RequestSchemaV1),
            other => Err(D::Error::custom(format!(
                "unsupported request schema '{other}'"
            ))),
        }
    }
}

/// A parsed, validated build request: the content-addressed store path plus the
/// table of recipe nodes to build (see the crate docs for the request shape).
/// Construct it with [`Request::parse_json`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    // Validated at deserialization via RequestSchemaV1; never read afterwards.
    #[allow(dead_code)]
    pub(crate) schema: RequestSchemaV1,
    pub(crate) store: PathBuf,
    pub(crate) quiet: Option<bool>,
    pub(crate) jobs: Option<usize>,
    pub(crate) nodes: BTreeMap<String, Value>,
}

impl Request {
    /// Parses and validates a request from its JSON encoding: enforces the
    /// request schema and that a `root` node exists. Returns
    /// [`ExecutionError::RequestLoad`] on malformed input.
    pub fn parse_json(bytes: &[u8]) -> Result<Self, ExecutionError> {
        let request: Request = serde_json::from_slice(bytes).map_err(|error| {
            ExecutionError::RequestLoad(format!("failed to decode request JSON value: {error}"))
        })?;
        validate_nodes(&request.nodes, "$.nodes")?;
        Ok(request)
    }
}

/// Validates a parsed node map: a `root` node must exist and every node must be
/// an object. Per-node fields are interpreted later, during graph collection.
fn validate_nodes(nodes: &BTreeMap<String, Value>, path: &str) -> Result<(), ExecutionError> {
    if !nodes.contains_key("root") {
        return Err(ExecutionError::RequestLoad(
            "missing required top-level node 'root'".to_string(),
        ));
    }
    for (node_id, node_value) in nodes {
        if !node_value.is_object() {
            return Err(ExecutionError::RequestLoad(format!(
                "{path}.{node_id}: expected request object"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn parse_request_nodes(
    value: Value,
    path: &str,
) -> Result<BTreeMap<String, Value>, ExecutionError> {
    let object = value.as_object().cloned().ok_or_else(|| {
        ExecutionError::RequestLoad(format!(
            "{path}: expected top-level object of node definitions"
        ))
    })?;
    let nodes: BTreeMap<String, Value> = object.into_iter().collect();
    validate_nodes(&nodes, path)?;
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_requires_top_level_root_node() {
        let error = parse_request_nodes(json!({"kind":"Legacy"}), "$").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing required top-level node 'root'"),
            "{error}"
        );
    }

    #[test]
    fn old_nested_root_shape_is_rejected() {
        let old_shape = json!({
            "name": "hello",
            "tag": "Tree",
            "config": {
                "tree": {
                    "entries": [{
                        "type": "file",
                        "path": "hello.txt",
                        "text": "hi",
                        "executable": false
                    }]
                }
            },
            "inputs": {}
        });

        let error =
            Request::parse_json(serde_json::to_vec(&old_shape).unwrap().as_slice()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to decode request JSON value"),
            "{error}"
        );
    }
}
