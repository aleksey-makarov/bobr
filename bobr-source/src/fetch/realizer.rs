//! Source-only milestone of the asynchronous DAG realizer.
//!
//! The current fetch request is a flat list, so every source is a goal and
//! there are no dependency edges yet. This module owns an in-flight table keyed
//! by declared object hash; each unique source is one async task whose
//! compiler-generated future state machine carries the acquisition control
//! flow. Later DAG and builder milestones extend this scheduler instead of
//! replacing the fetcher's Tokio engine.

use crate::fetch::engine::{Engine, SourceOutcome, process_source, record_source_aliases};
use crate::fetch::request::SourceEntry;
use bobr_core::ObjectHash;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::task::JoinSet;

struct SourceNode {
    declared: Option<ObjectHash>,
    primary: SourceEntry,
    aliases: Vec<String>,
}

/// Source-only Realizer built on the fetcher's existing Tokio acquisition engine.
pub(super) struct SourceRealizer {
    engine: Arc<Engine>,
    nodes: Vec<SourceNode>,
}

impl SourceRealizer {
    pub(super) fn new(engine: Arc<Engine>, entries: Vec<SourceEntry>) -> Self {
        let mut nodes = Vec::<SourceNode>::new();
        let mut in_flight = HashMap::<ObjectHash, usize>::new();
        for entry in entries {
            let declared = ObjectHash::from_str(entry.object_hash.trim()).ok();
            if let Some(hash) = declared
                && let Some(existing) = in_flight.get(&hash).copied()
            {
                nodes[existing].aliases.push(entry.name);
                continue;
            }
            let index = nodes.len();
            if let Some(hash) = declared {
                in_flight.insert(hash, index);
            }
            nodes.push(SourceNode {
                declared,
                primary: entry,
                aliases: Vec::new(),
            });
        }
        Self { engine, nodes }
    }

    pub(super) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub(super) async fn run(self) -> Result<Vec<SourceOutcome>, String> {
        let node_count = self.nodes.len();
        let mut tasks = JoinSet::new();
        for (index, node) in self.nodes.into_iter().enumerate() {
            let engine = self.engine.clone();
            tasks.spawn(async move {
                let outcome = process_source(engine.clone(), node.primary).await;
                if outcome_is_ready(&outcome)
                    && let Some(declared) = node.declared
                    && let Err(message) =
                        record_source_aliases(engine, declared, node.aliases).await
                {
                    return (
                        index,
                        SourceOutcome::Failed {
                            name: declared.to_string(),
                            message,
                        },
                    );
                }
                (index, outcome)
            });
        }

        let mut outcomes = (0..node_count).map(|_| None).collect::<Vec<_>>();
        while let Some(joined) = tasks.join_next().await {
            let (index, outcome) =
                joined.map_err(|error| format!("source realizer task panicked: {error}"))?;
            outcomes[index] = Some(outcome);
        }
        Ok(outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every source realizer task completes"))
            .collect())
    }
}

fn outcome_is_ready(outcome: &SourceOutcome) -> bool {
    matches!(
        outcome,
        SourceOutcome::Downloaded
            | SourceOutcome::CacheHit
            | SourceOutcome::Local
            | SourceOutcome::Secondary
    )
}
