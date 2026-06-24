use crate::runtime::RuntimeError;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RequestEnvelope {
    pub options: RequestOptions,
    pub request: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub store: Option<PathBuf>,
    pub quiet: Option<bool>,
    pub jobs: Option<usize>,
}

impl RequestEnvelope {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            RuntimeError::RequestLoad(format!("failed to decode request JSON value: {error}"))
        })?;
        parse_request_envelope(value, "$")
    }
}

fn parse_request_envelope(value: Value, path: &str) -> Result<RequestEnvelope, RuntimeError> {
    let mut object = value.as_object().cloned().ok_or_else(|| {
        RuntimeError::RequestLoad(format!("{path}: expected top-level request object"))
    })?;

    let options = match object.remove("options") {
        Some(value) => parse_request_options(value, &format!("{path}.options"))?,
        None => RequestOptions::default(),
    };
    let request = parse_request_nodes(
        object.remove("nodes").ok_or_else(|| {
            RuntimeError::RequestLoad(format!("{path}: missing required field 'nodes'"))
        })?,
        &format!("{path}.nodes"),
    )?;
    if !object.is_empty() {
        return Err(RuntimeError::RequestLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    Ok(RequestEnvelope { options, request })
}

fn parse_request_options(value: Value, path: &str) -> Result<RequestOptions, RuntimeError> {
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RequestLoad(format!("{path}: expected object")))?;

    let store = match object.remove("store") {
        Some(Value::String(value)) => Some(PathBuf::from(value)),
        Some(_) => {
            return Err(RuntimeError::RequestLoad(format!(
                "{path}.store: expected string"
            )));
        }
        None => None,
    };
    let quiet = match object.remove("quiet") {
        Some(Value::Bool(value)) => Some(value),
        Some(_) => {
            return Err(RuntimeError::RequestLoad(format!(
                "{path}.quiet: expected boolean"
            )));
        }
        None => None,
    };
    let jobs = match object.remove("jobs") {
        Some(Value::Number(value)) => {
            let jobs = value.as_u64().ok_or_else(|| {
                RuntimeError::RequestLoad(format!("{path}.jobs: expected non-negative integer"))
            })?;
            let jobs = usize::try_from(jobs).map_err(|_| {
                RuntimeError::RequestLoad(format!(
                    "{path}.jobs: value is too large for this platform"
                ))
            })?;
            Some(jobs)
        }
        Some(_) => {
            return Err(RuntimeError::RequestLoad(format!(
                "{path}.jobs: expected integer"
            )));
        }
        None => None,
    };
    if !object.is_empty() {
        return Err(RuntimeError::RequestLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(RequestOptions { store, quiet, jobs })
}

pub(crate) fn parse_request_nodes(
    value: Value,
    path: &str,
) -> Result<BTreeMap<String, Value>, RuntimeError> {
    let object = value.as_object().cloned().ok_or_else(|| {
        RuntimeError::RequestLoad(format!(
            "{path}: expected top-level object of node definitions"
        ))
    })?;

    if !object.contains_key("root") {
        return Err(RuntimeError::RequestLoad(
            "missing required top-level node 'root'".to_string(),
        ));
    }

    let mut nodes = BTreeMap::new();
    for (node_id, node_value) in object {
        let node_path = format!("{path}.{node_id}");
        nodes.insert(node_id, parse_request_node(node_value, &node_path)?);
    }

    Ok(nodes)
}

fn parse_request_node(value: Value, path: &str) -> Result<Value, RuntimeError> {
    value
        .as_object()
        .ok_or_else(|| RuntimeError::RequestLoad(format!("{path}: expected request object")))?;
    Ok(value)
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

        let error = RequestEnvelope::parse_json(serde_json::to_vec(&old_shape).unwrap().as_slice())
            .unwrap_err();
        assert!(
            error.to_string().contains("missing required field 'nodes'"),
            "{error}"
        );
    }
}
