//! Shared request-DAG planning for the asynchronous Realizer and legacy executor.

use crate::{SourcePlannedSubject, parse_source_subject};
use bobr_builder::{Builder, BuilderPlanError, BuilderPlannedSubject};
use bobr_core::BuildKey;
use bobr_store::validate_ref_name;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

/// Planned source or builder node keyed by its stable build identity.
pub enum PlannedNode {
    /// Source leaf with a declared object hash and optional origin.
    Source(SourcePlannedSubject),
    /// Builder with validated configuration and build-key inputs.
    Builder(BuilderPlannedSubject),
}

impl fmt::Debug for PlannedNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(subject) => formatter
                .debug_tuple("Source")
                .field(&subject.name())
                .finish(),
            Self::Builder(subject) => formatter.debug_tuple("Builder").field(subject).finish(),
        }
    }
}

impl PlannedNode {
    /// Returns the recipe name used for diagnostics and refs.
    pub fn name(&self) -> &str {
        match self {
            Self::Source(subject) => subject.name(),
            Self::Builder(subject) => subject.name(),
        }
    }

    /// Returns the source or builder tag.
    pub fn tag(&self) -> &str {
        match self {
            Self::Source(subject) => subject.tag(),
            Self::Builder(subject) => subject.tag(),
        }
    }

    /// Returns the node's stable build key.
    pub fn build_key(&self) -> BuildKey {
        match self {
            Self::Source(subject) => subject.build_key(),
            Self::Builder(subject) => subject.build_key(),
        }
    }

    /// Returns the builder variant, if this is a builder node.
    pub fn as_builder(&self) -> Option<&BuilderPlannedSubject> {
        match self {
            Self::Source(_) => None,
            Self::Builder(subject) => Some(subject),
        }
    }

    /// Returns the source variant, if this is a source node.
    pub fn as_source(&self) -> Option<&SourcePlannedSubject> {
        match self {
            Self::Source(subject) => Some(subject),
            Self::Builder(_) => None,
        }
    }
}

/// Planned reachable graph with an ordered, non-empty set of goals.
#[derive(Debug)]
pub struct PlannedGraph {
    goals: Vec<BuildKey>,
    nodes: HashMap<BuildKey, Arc<PlannedNode>>,
}

impl PlannedGraph {
    /// Returns goal build keys in request order.
    pub fn goals(&self) -> &[BuildKey] {
        &self.goals
    }

    /// Returns all reachable nodes deduplicated by build key.
    pub fn nodes(&self) -> &HashMap<BuildKey, Arc<PlannedNode>> {
        &self.nodes
    }

    /// Returns one reachable node by build key.
    pub fn node(&self, key: BuildKey) -> Option<&Arc<PlannedNode>> {
        self.nodes.get(&key)
    }
}

/// Error category retained when the legacy executor maps planning failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphPlanErrorKind {
    /// Malformed request JSON shape or recipe fields.
    RequestLoad,
    /// Builder tag is not registered.
    UnknownBuilder,
    /// Structurally invalid graph or identity computation.
    InvalidRequest,
}

/// Request-DAG planning failure with a stable user-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPlanError {
    kind: GraphPlanErrorKind,
    message: String,
}

impl GraphPlanError {
    fn new(kind: GraphPlanErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Returns the error category.
    pub fn kind(&self) -> GraphPlanErrorKind {
        self.kind
    }

    /// Returns the diagnostic message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for GraphPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GraphPlanError {}

/// Plans the reachable subgraph for ordered goal node IDs.
///
/// Goals must be non-empty, unique, and present in `nodes`. Unreachable node
/// values are deliberately not parsed. Nodes reached through different IDs but
/// producing the same build key share the first planned representative.
pub fn plan_graph(
    nodes: &BTreeMap<String, Value>,
    goal_ids: &[String],
) -> Result<PlannedGraph, GraphPlanError> {
    if goal_ids.is_empty() {
        return Err(GraphPlanError::new(
            GraphPlanErrorKind::InvalidRequest,
            "request graph requires at least one goal",
        ));
    }
    let mut seen_goals = HashSet::new();
    for goal in goal_ids {
        if !seen_goals.insert(goal.as_str()) {
            return Err(GraphPlanError::new(
                GraphPlanErrorKind::InvalidRequest,
                format!("request graph contains duplicate goal node id '{goal}'"),
            ));
        }
        if !nodes.contains_key(goal) {
            return Err(GraphPlanError::new(
                GraphPlanErrorKind::InvalidRequest,
                format!("request goal references unknown node id '{goal}'"),
            ));
        }
    }

    let mut planned = Planner {
        request_nodes: nodes,
        planned_nodes: HashMap::new(),
        node_keys: HashMap::new(),
        visited_in_path: BTreeSet::new(),
    };
    let goals = goal_ids
        .iter()
        .map(|goal| planned.collect(goal))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PlannedGraph {
        goals,
        nodes: planned.planned_nodes,
    })
}

struct Planner<'a> {
    request_nodes: &'a BTreeMap<String, Value>,
    planned_nodes: HashMap<BuildKey, Arc<PlannedNode>>,
    node_keys: HashMap<String, BuildKey>,
    visited_in_path: BTreeSet<String>,
}

impl Planner<'_> {
    fn collect(&mut self, node_id: &str) -> Result<BuildKey, GraphPlanError> {
        if let Some(existing) = self.node_keys.get(node_id) {
            return Ok(*existing);
        }
        if !self.visited_in_path.insert(node_id.to_string()) {
            return Err(GraphPlanError::new(
                GraphPlanErrorKind::InvalidRequest,
                format!("request graph contains a cycle through node id '{node_id}'"),
            ));
        }

        let value = self.request_nodes.get(node_id).ok_or_else(|| {
            GraphPlanError::new(
                GraphPlanErrorKind::InvalidRequest,
                format!("request references unknown node id '{node_id}'"),
            )
        })?;
        let path = format!("$.nodes.{node_id}");
        let mut object = value.as_object().cloned().ok_or_else(|| {
            GraphPlanError::new(
                GraphPlanErrorKind::RequestLoad,
                format!("{path}: expected request object"),
            )
        })?;
        let tag = take_string(&mut object, &path, "tag")?;

        let (key, node) = if tag == "Source" {
            let subject = parse_source_subject(object).map_err(|error| {
                GraphPlanError::new(GraphPlanErrorKind::RequestLoad, format!("{path}: {error}"))
            })?;
            (subject.build_key(), Arc::new(PlannedNode::Source(subject)))
        } else {
            let inputs_value = object.remove("inputs").ok_or_else(|| {
                GraphPlanError::new(
                    GraphPlanErrorKind::RequestLoad,
                    format!("{path}: missing required field 'inputs'"),
                )
            })?;
            let inputs_object = inputs_value.as_object().cloned().ok_or_else(|| {
                GraphPlanError::new(
                    GraphPlanErrorKind::RequestLoad,
                    format!("{path}.inputs: expected object"),
                )
            })?;
            let mut inputs = BTreeMap::new();
            for (input_name, input_value) in inputs_object {
                let input_path = format!("{path}.inputs.{input_name}");
                let child_id = parse_input_value(input_value, &input_path)?;
                inputs.insert(input_name, self.collect(&child_id)?);
            }
            let subject = parse_builder_subject(&tag, object, inputs)
                .map_err(|error| map_builder_error(error, &path))?;
            (subject.build_key(), Arc::new(PlannedNode::Builder(subject)))
        };

        validate_ref_name(node.name()).map_err(|error| {
            GraphPlanError::new(GraphPlanErrorKind::RequestLoad, format!("{path}: {error}"))
        })?;
        self.visited_in_path.remove(node_id);
        self.planned_nodes
            .entry(key)
            .or_insert_with(|| node.clone());
        self.node_keys.insert(node_id.to_string(), key);
        Ok(key)
    }
}

/// Parses one builder against the single in-tree registry.
///
/// Public only so the legacy executor's focused registry tests exercise the
/// same planner during migration.
pub fn parse_builder_subject(
    tag: &str,
    mut object: Map<String, Value>,
    inputs: BTreeMap<String, BuildKey>,
) -> Result<BuilderPlannedSubject, BuilderPlanError> {
    let name = object
        .remove("name")
        .ok_or_else(|| BuilderPlanError::recipe("missing required field 'name'"))?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| BuilderPlanError::recipe("name: expected string"))?;
    let config = object
        .remove("config")
        .ok_or_else(|| BuilderPlanError::recipe("missing required field 'config'"))?;
    if !object.is_empty() {
        return Err(BuilderPlanError::recipe(format!(
            "unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    let builder = registered_builders()
        .find(|builder| builder.tag().eq_ignore_ascii_case(tag))
        .ok_or_else(|| BuilderPlanError::UnknownBuilder {
            tag: tag.to_string(),
            supported_tags: registered_builders().map(|builder| builder.tag()).collect(),
        })?;
    BuilderPlannedSubject::new(builder, name, config, inputs)
}

fn registered_builders() -> impl Iterator<Item = &'static dyn Builder> {
    bobr_builder::BUILDERS
        .iter()
        .copied()
        .chain(bobr_sandbox::BUILDERS.iter().copied())
}

fn map_builder_error(error: BuilderPlanError, path: &str) -> GraphPlanError {
    let message = format!("{path}: {error}");
    let kind = match error {
        BuilderPlanError::UnknownBuilder { .. } => GraphPlanErrorKind::UnknownBuilder,
        BuilderPlanError::Recipe(_) => GraphPlanErrorKind::RequestLoad,
        BuilderPlanError::InvalidRequest(_) | BuilderPlanError::Identity(_) => {
            GraphPlanErrorKind::InvalidRequest
        }
    };
    GraphPlanError::new(kind, message)
}

fn parse_input_value(value: Value, path: &str) -> Result<String, GraphPlanError> {
    match value {
        Value::String(child) => Ok(child),
        Value::Null => Err(GraphPlanError::new(
            GraphPlanErrorKind::RequestLoad,
            format!("{path}: expected node id string, got null"),
        )),
        Value::Array(_) => Err(GraphPlanError::new(
            GraphPlanErrorKind::RequestLoad,
            format!("{path}: expected node id string, got array"),
        )),
        _ => Err(GraphPlanError::new(
            GraphPlanErrorKind::RequestLoad,
            format!("{path}: expected node id string"),
        )),
    }
}

fn take_string(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<String, GraphPlanError> {
    object
        .remove(field)
        .ok_or_else(|| {
            GraphPlanError::new(
                GraphPlanErrorKind::RequestLoad,
                format!("{path}: missing required field '{field}'"),
            )
        })?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            GraphPlanError::new(
                GraphPlanErrorKind::RequestLoad,
                format!("{path}.{field}: expected string"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(name: &str, text: &str) -> Value {
        json!({
            "name": name,
            "tag": "Tree",
            "config": {
                "tree": { "entries": [{
                    "type": "file", "path": "value", "text": text,
                    "executable": false
                }] }
            },
            "inputs": {}
        })
    }

    #[test]
    fn multiple_goals_share_nodes_by_build_key_and_preserve_order() {
        let nodes = BTreeMap::from([
            ("first".to_string(), tree("same", "value")),
            ("second".to_string(), tree("same", "value")),
        ]);
        let graph = plan_graph(&nodes, &["second".to_string(), "first".to_string()]).unwrap();

        assert_eq!(graph.goals().len(), 2);
        assert_eq!(graph.goals()[0], graph.goals()[1]);
        assert_eq!(graph.nodes().len(), 1);
    }

    #[test]
    fn goals_must_be_nonempty_unique_and_known() {
        let nodes = BTreeMap::from([("root".to_string(), tree("root", "value"))]);
        assert!(
            plan_graph(&nodes, &[])
                .unwrap_err()
                .to_string()
                .contains("at least one goal")
        );
        assert!(
            plan_graph(&nodes, &["root".to_string(), "root".to_string()])
                .unwrap_err()
                .to_string()
                .contains("duplicate goal")
        );
        assert!(
            plan_graph(&nodes, &["missing".to_string()])
                .unwrap_err()
                .to_string()
                .contains("unknown node")
        );
    }

    #[test]
    fn unreachable_nodes_are_not_parsed() {
        let nodes = BTreeMap::from([
            ("root".to_string(), tree("root", "value")),
            ("broken".to_string(), json!({"tag": []})),
        ]);
        let graph = plan_graph(&nodes, &["root".to_string()]).unwrap();
        assert_eq!(graph.nodes().len(), 1);
    }
}
