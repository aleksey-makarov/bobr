//! Lazy exact resolution of a planned multi-goal DAG.
//!
//! This milestone resolves working and trusted-secondary build handles before
//! traversing inputs. Exact hits therefore prune dependency subtrees. Nodes
//! that miss are returned as source and builder frontiers. Builder misses can
//! now cross the in-process executor boundary; dynamic reuse is added by the
//! next Realizer stage.

use crate::build_executor::{
    BuildExecutorError, BuildExecutorHandle, BuilderJob, PublishedBuilderOutput,
    publish_builder_output,
};
use crate::graph::PlannedGraph;
use bobr_core::{BuildKey, ObjectHash};
use bobr_store::{SecondaryResolution, SecondaryResolver, Store, StoreError, load_build_handle};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

/// Result of lazy exact resolution before reuse or builder execution.
#[derive(Debug)]
pub struct LazyExactPlan {
    goals: Vec<BuildKey>,
    resolved: HashMap<BuildKey, ObjectHash>,
    source_frontier: Vec<BuildKey>,
    builder_frontier: Vec<BuildKey>,
    secondary_reports: Vec<SecondaryResolution<BuildKey>>,
}

impl LazyExactPlan {
    /// Returns goal build keys in request order.
    pub fn goals(&self) -> &[BuildKey] {
        &self.goals
    }

    /// Returns exact results found in the working or secondary stores.
    pub fn resolved(&self) -> &HashMap<BuildKey, ObjectHash> {
        &self.resolved
    }

    /// Returns exact-miss Source leaves in first-discovery order.
    pub fn source_frontier(&self) -> &[BuildKey] {
        &self.source_frontier
    }

    /// Returns exact-miss builders whose inputs were traversed.
    ///
    /// This is a discovered frontier, not yet a ready-to-run queue; input
    /// realization, reuse, and BuildExecutor scheduling come later.
    pub fn builder_frontier(&self) -> &[BuildKey] {
        &self.builder_frontier
    }

    /// Returns secondary lookup reports, including misses and conflicts.
    ///
    /// Reports retain index names and candidate hashes as structured data so
    /// the run layer can emit conflict warnings without parsing text.
    pub fn secondary_reports(&self) -> &[SecondaryResolution<BuildKey>] {
        &self.secondary_reports
    }
}

/// Failure while reading exact mappings or traversing a planned graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyExactError {
    message: String,
}

impl LazyExactError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LazyExactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LazyExactError {}

impl From<StoreError> for LazyExactError {
    fn from(error: StoreError) -> Self {
        Self::new(error.to_string())
    }
}

/// Executes one already-decided builder miss and publishes its staged output.
///
/// Inputs and the deterministic reuse key must already be available in `job`.
/// Reuse lookup belongs immediately before this function and is added by the
/// next Realizer stage. The synchronous builder runs under `jobs`; the final
/// store import runs on Tokio's blocking pool after the builder slot is freed.
pub async fn execute_builder_miss(
    executor: &BuildExecutorHandle,
    job: BuilderJob,
    working: Store,
) -> Result<PublishedBuilderOutput, BuildExecutorError> {
    let staged = executor.submit(job).await?.wait().await?;
    tokio::task::spawn_blocking(move || publish_builder_output(&working, staged))
        .await
        .map_err(|error| {
            BuildExecutorError::Panic(format!("builder publication task panicked: {error}"))
        })?
}

/// Resolves exact build handles lazily from all goals.
///
/// Each round checks working mappings first, then queries trusted secondary
/// indexes for the remaining batch. Only complete exact misses have their
/// immediate inputs enqueued. All filesystem and hardlink work runs on Tokio's
/// blocking pool rather than an async runtime worker.
pub async fn resolve_lazy_exact(
    graph: &PlannedGraph,
    secondary: Arc<SecondaryResolver>,
) -> Result<LazyExactPlan, LazyExactError> {
    let mut pending = graph.goals().iter().copied().collect::<VecDeque<_>>();
    let mut visited = HashSet::new();
    let mut resolved = HashMap::new();
    let mut source_frontier = Vec::new();
    let mut builder_frontier = Vec::new();
    let mut secondary_reports = Vec::new();

    while !pending.is_empty() {
        let mut batch = Vec::new();
        while let Some(key) = pending.pop_front() {
            if visited.insert(key) {
                batch.push(key);
            }
        }
        if batch.is_empty() {
            continue;
        }

        let working = secondary.working().clone();
        let lookup_keys = batch.clone();
        let working_results = tokio::task::spawn_blocking(move || {
            lookup_keys
                .into_iter()
                .map(|key| load_build_handle(&working, key).map(|result| (key, result)))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(|error| {
            LazyExactError::new(format!("working exact lookup panicked: {error}"))
        })??;

        let mut misses = Vec::new();
        for (key, result) in working_results {
            if let Some(object_hash) = result {
                resolved.insert(key, object_hash);
            } else {
                misses.push(key);
            }
        }

        let mut unresolved = misses;
        if secondary.has_trusted_indexes() && !unresolved.is_empty() {
            let resolver = secondary.clone();
            let query = unresolved.clone();
            let reports = tokio::task::spawn_blocking(move || resolver.resolve_builds(&query))
                .await
                .map_err(|error| {
                    LazyExactError::new(format!("secondary exact lookup panicked: {error}"))
                })??;
            unresolved.clear();
            for report in reports {
                if let Some(hit) = &report.resolved {
                    resolved.insert(report.key, hit.object_hash);
                } else {
                    unresolved.push(report.key);
                }
                secondary_reports.push(report);
            }
        }

        for key in unresolved {
            let node = graph.node(key).ok_or_else(|| {
                LazyExactError::new(format!(
                    "planned graph is missing reachable node for build key '{key}'"
                ))
            })?;
            if let Some(builder) = node.as_builder() {
                builder_frontier.push(key);
                for dependency in builder.inputs().values() {
                    if !visited.contains(dependency) {
                        pending.push_back(*dependency);
                    }
                }
            } else {
                source_frontier.push(key);
            }
        }
    }

    Ok(LazyExactPlan {
        goals: graph.goals().to_vec(),
        resolved,
        source_frontier,
        builder_frontier,
        secondary_reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::plan_graph;
    use bobr_core::ReuseKey;
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use bobr_store::{
        LocalHardlinkContentSource, LocalTrustedKeyIndex, NamedContentSource, NamedTrustedKeyIndex,
        ReadOnlyStore, Store, import_build,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn source(name: &str, hash_digit: char) -> Value {
        json!({
            "name": name,
            "tag": "Source",
            "object_hash": hash_digit.to_string().repeat(64)
        })
    }

    fn group(name: &str, input_ids: &[(&str, &str)]) -> Value {
        json!({
            "name": name,
            "tag": "Group",
            "config": {},
            "inputs": input_ids.iter().copied().collect::<BTreeMap<_, _>>()
        })
    }

    fn store(path: &Path) -> Store {
        fs::create_dir(path).unwrap();
        Store::create(path).unwrap()
    }

    fn publish(store: &Store, build_key: BuildKey, staged: &Path, bytes: &[u8]) -> ObjectHash {
        fs::write(staged, bytes).unwrap();
        import_build(
            store,
            build_key,
            ReuseKey::from_str(&"9".repeat(64)).unwrap(),
            Vec::new(),
            staged,
            &format!("test-{build_key}"),
            "test-run",
        )
        .unwrap()
    }

    fn empty_resolver(working: Store) -> Arc<SecondaryResolver> {
        Arc::new(SecondaryResolver::new(working, "test-run", Vec::new(), Vec::new()).unwrap())
    }

    #[tokio::test]
    async fn working_exact_goal_prunes_its_dependency_subtree() {
        let temp = tempdir().unwrap();
        let graph = plan_graph(
            &BTreeMap::from([
                ("src".to_string(), source("src", '1')),
                ("root".to_string(), group("root", &[("dependency", "src")])),
            ]),
            &["root".to_string()],
        )
        .unwrap();
        let working = store(&temp.path().join("working"));
        let object_hash = publish(
            &working,
            graph.goals()[0],
            &temp.path().join("working-object"),
            b"working exact\n",
        );

        let plan = resolve_lazy_exact(&graph, empty_resolver(working))
            .await
            .unwrap();

        assert_eq!(plan.resolved().get(&graph.goals()[0]), Some(&object_hash));
        assert!(plan.source_frontier().is_empty());
        assert!(plan.builder_frontier().is_empty());
        assert!(plan.secondary_reports().is_empty());
    }

    #[tokio::test]
    async fn one_goal_hit_does_not_prune_another_goals_missed_subtree() {
        let temp = tempdir().unwrap();
        let graph = plan_graph(
            &BTreeMap::from([
                ("pruned".to_string(), source("pruned", '4')),
                ("needed".to_string(), source("needed", '5')),
                (
                    "hit".to_string(),
                    group("hit", &[("pruned_input", "pruned")]),
                ),
                (
                    "miss".to_string(),
                    group("miss", &[("needed_input", "needed")]),
                ),
            ]),
            &["hit".to_string(), "miss".to_string()],
        )
        .unwrap();
        let working = store(&temp.path().join("working"));
        publish(
            &working,
            graph.goals()[0],
            &temp.path().join("hit-object"),
            b"first goal exact\n",
        );
        let needed_key = BuildKey::from_object_hash(ObjectHash::from_str(&"5".repeat(64)).unwrap());
        let pruned_key = BuildKey::from_object_hash(ObjectHash::from_str(&"4".repeat(64)).unwrap());

        let plan = resolve_lazy_exact(&graph, empty_resolver(working))
            .await
            .unwrap();

        assert!(plan.resolved().contains_key(&graph.goals()[0]));
        assert_eq!(plan.builder_frontier(), &graph.goals()[1..]);
        assert_eq!(plan.source_frontier(), &[needed_key]);
        assert!(!plan.source_frontier().contains(&pruned_key));
    }

    #[tokio::test]
    async fn secondary_exact_goal_is_imported_and_prunes_dependencies() {
        let temp = tempdir().unwrap();
        let graph = plan_graph(
            &BTreeMap::from([
                ("src".to_string(), source("src", '2')),
                ("root".to_string(), group("root", &[("input", "src")])),
            ]),
            &["root".to_string()],
        )
        .unwrap();
        let secondary_root = temp.path().join("secondary");
        let secondary_store = store(&secondary_root);
        let object_hash = publish(
            &secondary_store,
            graph.goals()[0],
            &temp.path().join("secondary-object"),
            b"secondary exact\n",
        );
        let working = store(&temp.path().join("working"));
        let read_only = ReadOnlyStore::open(&secondary_root).unwrap();
        let resolver = Arc::new(
            SecondaryResolver::new(
                working.clone(),
                "test-run",
                vec![NamedTrustedKeyIndex::new(
                    "secondary",
                    Arc::new(LocalTrustedKeyIndex::new(read_only.clone())),
                )],
                vec![NamedContentSource::new(
                    "secondary",
                    Arc::new(LocalHardlinkContentSource::with_runtime(
                        read_only,
                        RuntimeProvider::host(),
                    )),
                )],
            )
            .unwrap(),
        );

        let plan = resolve_lazy_exact(&graph, resolver).await.unwrap();

        assert_eq!(plan.resolved().get(&graph.goals()[0]), Some(&object_hash));
        assert_eq!(
            load_build_handle(&working, graph.goals()[0]).unwrap(),
            Some(object_hash)
        );
        assert!(plan.source_frontier().is_empty());
        assert!(plan.builder_frontier().is_empty());
        assert_eq!(plan.secondary_reports().len(), 1);
        assert_eq!(
            plan.secondary_reports()[0]
                .resolved
                .as_ref()
                .unwrap()
                .content_sources,
            ["secondary"]
        );
    }

    #[tokio::test]
    async fn secondary_index_conflict_is_retained_as_structured_data() {
        let temp = tempdir().unwrap();
        let graph = plan_graph(
            &BTreeMap::from([("root".to_string(), group("root", &[]))]),
            &["root".to_string()],
        )
        .unwrap();
        let first_root = temp.path().join("first");
        let first_store = store(&first_root);
        let first_hash = publish(
            &first_store,
            graph.goals()[0],
            &temp.path().join("first-object"),
            b"first candidate\n",
        );
        let second_root = temp.path().join("second");
        let second_store = store(&second_root);
        let second_hash = publish(
            &second_store,
            graph.goals()[0],
            &temp.path().join("second-object"),
            b"second candidate\n",
        );
        let first = ReadOnlyStore::open(&first_root).unwrap();
        let second = ReadOnlyStore::open(&second_root).unwrap();
        let resolver = Arc::new(
            SecondaryResolver::new(
                store(&temp.path().join("working")),
                "test-run",
                vec![
                    NamedTrustedKeyIndex::new(
                        "first-index",
                        Arc::new(LocalTrustedKeyIndex::new(first.clone())),
                    ),
                    NamedTrustedKeyIndex::new(
                        "second-index",
                        Arc::new(LocalTrustedKeyIndex::new(second.clone())),
                    ),
                ],
                vec![
                    NamedContentSource::new(
                        "first-content",
                        Arc::new(LocalHardlinkContentSource::with_runtime(
                            first,
                            RuntimeProvider::host(),
                        )),
                    ),
                    NamedContentSource::new(
                        "second-content",
                        Arc::new(LocalHardlinkContentSource::with_runtime(
                            second,
                            RuntimeProvider::host(),
                        )),
                    ),
                ],
            )
            .unwrap(),
        );

        let plan = resolve_lazy_exact(&graph, resolver).await.unwrap();
        let report = &plan.secondary_reports()[0];

        assert!(report.has_conflict());
        assert_eq!(
            report
                .answers
                .iter()
                .map(|answer| (answer.index.as_str(), answer.object_hash))
                .collect::<Vec<_>>(),
            [("first-index", first_hash), ("second-index", second_hash)]
        );
        assert_eq!(report.resolved.as_ref().unwrap().object_hash, first_hash);
        assert_eq!(plan.resolved().get(&graph.goals()[0]), Some(&first_hash));
    }

    #[tokio::test]
    async fn misses_expand_multiple_goals_and_deduplicate_shared_dependencies() {
        let graph = plan_graph(
            &BTreeMap::from([
                ("shared".to_string(), source("shared", '3')),
                (
                    "first".to_string(),
                    group("first", &[("first_input", "shared")]),
                ),
                (
                    "second".to_string(),
                    group("second", &[("second_input", "shared")]),
                ),
            ]),
            &["second".to_string(), "first".to_string()],
        )
        .unwrap();
        let temp = tempdir().unwrap();
        let working = store(&temp.path().join("working"));

        let plan = resolve_lazy_exact(&graph, empty_resolver(working))
            .await
            .unwrap();

        assert_eq!(plan.goals(), graph.goals());
        assert!(plan.resolved().is_empty());
        assert_eq!(plan.builder_frontier(), graph.goals());
        assert_eq!(plan.source_frontier().len(), 1);
        assert_ne!(plan.source_frontier()[0], graph.goals()[0]);
        assert_ne!(plan.source_frontier()[0], graph.goals()[1]);
    }
}
