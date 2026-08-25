use crate::execution::ExecutionError;
use bobr_core::ProgressPolicy;
use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

/// The request format this build of `bobr` accepts.
///
/// It is the compatibility contract between `bobr` and whoever writes requests:
/// a recipe layer emitting a different schema is talking to the wrong version.
/// `bobr --version` reports it, so a caller can compare before building rather
/// than discovering the mismatch in the parse error.
pub const REQUEST_SCHEMA: &str = "bobr-request-v4";

/// Schema marker for the request format. It deserializes only from the exact
/// schema string, so the format version is enforced declaratively at parse
/// time and never needs to live as data on `Request`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestSchemaV4;

impl<'de> Deserialize<'de> for RequestSchemaV4 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            REQUEST_SCHEMA => Ok(RequestSchemaV4),
            other => Err(D::Error::custom(format!(
                "unsupported request schema '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalSecondary {
    pub(crate) name: String,
    pub(crate) store: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Secondaries {
    #[serde(default)]
    pub(crate) trusted_indexes: Vec<LocalSecondary>,
    #[serde(default)]
    pub(crate) content_sources: Vec<LocalSecondary>,
}

/// A parsed unified Realizer request.
///
/// The request names the working store and run, an ordered non-empty goal set,
/// the complete recipe-node graph, acquisition limits, and optional local
/// secondary capabilities. Construct it with [`Request::parse_json`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    // Validated at deserialization via RequestSchemaV4; never read afterwards.
    #[allow(dead_code)]
    pub(crate) schema: RequestSchemaV4,
    pub(crate) store: PathBuf,
    pub(crate) logs: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) run_id: String,
    pub(crate) quiet: Option<bool>,
    pub(crate) jobs: Option<usize>,
    #[serde(default)]
    pub(crate) progress: ProgressPolicy,
    #[serde(default)]
    pub(crate) limits: bobr_source::fetch::Limits,
    #[serde(default)]
    pub(crate) secondaries: Secondaries,
    pub(crate) goals: Vec<String>,
    pub(crate) nodes: BTreeMap<String, Value>,
}

impl Request {
    /// Parses and validates a request from its JSON encoding: enforces the
    /// request schema, ordered goals, local capabilities, and node shapes.
    /// Returns
    /// [`ExecutionError::RequestLoad`] on malformed input.
    pub fn parse_json(bytes: &[u8]) -> Result<Self, ExecutionError> {
        let request: Request = serde_json::from_slice(bytes).map_err(|error| {
            ExecutionError::RequestLoad(format!("failed to decode request JSON value: {error}"))
        })?;
        validate_nodes(&request.nodes, "$.nodes")?;
        validate_goals(&request.goals, &request.nodes)?;
        validate_limits(request.jobs, &request.limits)?;
        request
            .progress
            .validate()
            .map_err(ExecutionError::InvalidRequest)?;
        validate_secondaries(&request.secondaries)?;
        Ok(request)
    }
}

/// Validates that every node is an object. Per-node fields are interpreted
/// later, during graph planning.
fn validate_nodes(nodes: &BTreeMap<String, Value>, path: &str) -> Result<(), ExecutionError> {
    for (node_id, node_value) in nodes {
        if !node_value.is_object() {
            return Err(ExecutionError::RequestLoad(format!(
                "{path}.{node_id}: expected request object"
            )));
        }
    }
    Ok(())
}

fn validate_goals(goals: &[String], nodes: &BTreeMap<String, Value>) -> Result<(), ExecutionError> {
    if goals.is_empty() {
        return Err(ExecutionError::InvalidRequest(
            "request requires at least one goal".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for goal in goals {
        if !seen.insert(goal) {
            return Err(ExecutionError::InvalidRequest(format!(
                "request contains duplicate goal node id '{goal}'"
            )));
        }
        if !nodes.contains_key(goal) {
            return Err(ExecutionError::InvalidRequest(format!(
                "request goal references unknown node id '{goal}'"
            )));
        }
    }
    Ok(())
}

fn validate_limits(
    jobs: Option<usize>,
    limits: &bobr_source::fetch::Limits,
) -> Result<(), ExecutionError> {
    if jobs == Some(0) {
        return Err(ExecutionError::InvalidRequest(
            "request 'jobs' must be greater than zero".to_string(),
        ));
    }
    for (name, value) in [
        ("limits.per_host_default", limits.per_host_default),
        ("limits.max_connections", limits.max_connections),
        ("limits.max_local_jobs", limits.max_local_jobs),
    ] {
        if value == Some(0) {
            return Err(ExecutionError::InvalidRequest(format!(
                "request '{name}' must be greater than zero"
            )));
        }
    }
    if let Some((host, _)) = limits.per_host.iter().find(|(_, value)| **value == 0) {
        return Err(ExecutionError::InvalidRequest(format!(
            "request 'limits.per_host.{host}' must be greater than zero"
        )));
    }
    Ok(())
}

fn validate_secondaries(secondaries: &Secondaries) -> Result<(), ExecutionError> {
    for (kind, entries) in [
        ("trusted index", &secondaries.trusted_indexes),
        ("content source", &secondaries.content_sources),
    ] {
        let mut names = HashSet::new();
        for entry in entries {
            if entry.name.is_empty() {
                return Err(ExecutionError::InvalidRequest(format!(
                    "secondary {kind} name must not be empty"
                )));
            }
            if !names.insert(&entry.name) {
                return Err(ExecutionError::InvalidRequest(format!(
                    "duplicate secondary {kind} name '{}'",
                    entry.name
                )));
            }
            if !entry.store.is_absolute() {
                return Err(ExecutionError::InvalidRequest(format!(
                    "secondary {kind} '{}' store path must be absolute: '{}'",
                    entry.name,
                    entry.store.display()
                )));
            }
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
    fn request_names_the_run_and_its_directories() {
        let request = Request::parse_json(
            json!({
                "schema": "bobr-request-v4",
                "store": "/store",
                "logs": "/logs/run",
                "work": "/work/run",
                "run_id": "260803120000",
                "goals": ["root"],
                "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();

        assert_eq!(request.store, PathBuf::from("/store"));
        assert_eq!(request.logs, PathBuf::from("/logs/run"));
        assert_eq!(request.work, PathBuf::from("/work/run"));
        assert_eq!(request.run_id, "260803120000");
    }

    #[test]
    fn request_without_a_run_is_rejected() {
        let error = Request::parse_json(
            json!({
                "schema": "bobr-request-v4",
                "store": "/store",
                "goals": ["root"],
                "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("missing field `logs`"),
            "{error}"
        );
    }

    #[test]
    fn the_previous_request_schema_is_rejected() {
        let error = Request::parse_json(
            json!({
                "schema": "bobr-request-v3",
                "store": "/store",
                "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported request schema 'bobr-request-v3'"),
            "{error}"
        );
    }

    #[test]
    fn progress_policy_is_typed_defaulted_and_validated() {
        let base = json!({
            "schema": "bobr-request-v4",
            "store": "/store",
            "logs": "/logs/run",
            "work": "/work/run",
            "run_id": "run",
            "goals": ["root"],
            "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
        });
        let automatic = Request::parse_json(&serde_json::to_vec(&base).unwrap()).unwrap();
        assert_eq!(automatic.progress, ProgressPolicy::Auto);

        let mut summary = base.clone();
        summary["progress"] = json!({ "mode": "summary" });
        assert_eq!(
            Request::parse_json(&serde_json::to_vec(&summary).unwrap())
                .unwrap()
                .progress,
            ProgressPolicy::Summary
        );

        let mut fixed = base.clone();
        fixed["progress"] = json!({ "mode": "fixed", "max_lines": 8 });
        assert_eq!(
            Request::parse_json(&serde_json::to_vec(&fixed).unwrap())
                .unwrap()
                .progress,
            ProgressPolicy::Fixed { max_lines: 8 }
        );

        fixed["progress"]["max_lines"] = json!(3);
        assert!(
            Request::parse_json(&serde_json::to_vec(&fixed).unwrap())
                .unwrap_err()
                .to_string()
                .contains("at least 4")
        );

        let mut misspelled = base;
        misspelled["progress"] = json!({ "mode": "auto", "max_lines": 8 });
        assert!(
            Request::parse_json(&serde_json::to_vec(&misspelled).unwrap())
                .unwrap_err()
                .to_string()
                .contains("only in fixed mode")
        );
    }

    #[test]
    fn request_requires_nonempty_known_unique_goals() {
        let base = json!({
            "schema": "bobr-request-v4",
            "store": "/store",
            "logs": "/logs/run",
            "work": "/work/run",
            "run_id": "run",
            "nodes": { "node": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
        });
        let mut empty = base.clone();
        empty["goals"] = json!([]);
        assert!(Request::parse_json(&serde_json::to_vec(&empty).unwrap()).is_err());
        let mut unknown = base.clone();
        unknown["goals"] = json!(["missing"]);
        assert!(Request::parse_json(&serde_json::to_vec(&unknown).unwrap()).is_err());
        let mut duplicate = base;
        duplicate["goals"] = json!(["node", "node"]);
        assert!(Request::parse_json(&serde_json::to_vec(&duplicate).unwrap()).is_err());
    }

    #[test]
    fn request_validates_limits_and_local_secondary_capabilities() {
        let request = json!({
            "schema": "bobr-request-v4",
            "store": "/store",
            "logs": "/logs/run",
            "work": "/work/run",
            "run_id": "run",
            "goals": ["node"],
            "limits": {
                "per_host_default": 2,
                "per_host": { "example.test": 1 },
                "max_connections": 8,
                "max_local_jobs": 3
            },
            "secondaries": {
                "trusted_indexes": [{ "name": "old", "store": "/old" }],
                "content_sources": [{ "name": "old", "store": "/old" }]
            },
            "nodes": { "node": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
        });
        Request::parse_json(&serde_json::to_vec(&request).unwrap()).unwrap();

        let mut zero = request.clone();
        zero["limits"]["max_local_jobs"] = json!(0);
        assert!(
            Request::parse_json(&serde_json::to_vec(&zero).unwrap())
                .unwrap_err()
                .to_string()
                .contains("max_local_jobs")
        );
        let mut relative = request.clone();
        relative["secondaries"]["content_sources"][0]["store"] = json!("relative");
        assert!(
            Request::parse_json(&serde_json::to_vec(&relative).unwrap())
                .unwrap_err()
                .to_string()
                .contains("must be absolute")
        );
        let mut duplicate = request;
        duplicate["secondaries"]["trusted_indexes"] = json!([
            { "name": "old", "store": "/one" },
            { "name": "old", "store": "/two" }
        ]);
        assert!(
            Request::parse_json(&serde_json::to_vec(&duplicate).unwrap())
                .unwrap_err()
                .to_string()
                .contains("duplicate secondary trusted index")
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
