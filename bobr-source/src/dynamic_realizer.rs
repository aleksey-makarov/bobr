//! Demand-driven candidate resolution and dynamic reuse for a planned DAG.
//!
//! A node first resolves to an ordered set of object hashes. Trusted mappings
//! are not accompanied by content lookup, so these hashes can flow into parent
//! reuse keys without entering the working store. Content is acquired only for
//! goals and for inputs of a builder that reached a complete reuse miss.

use crate::acquisition::engine::{
    Engine as SourceEngine, SourceEntry, SourceOutcome, engine_for_dynamic_realizer, process_source,
};
use crate::build_executor::{
    BuildExecutorError, BuildExecutorHandle, BuilderExecution, BuilderJob,
};
use crate::graph::{PlannedGraph, PlannedNode};
use crate::realizer::execute_builder_miss;
use bobr_builder::{BuilderInputs, BuilderPlannedSubject, materialize_fs_tree_root};
use bobr_core::{
    BuildKey, BuildLogEvent, BuildLogLevel, BuildRunLogger, BuildStatus, CancellationToken,
    ObjectHash, ReuseKey, Run, RuntimeProvider, SubjectIdentity,
};
use bobr_store::{
    MappingCandidates, SecondaryResolver, Store, StoreError, load_build_object_hash,
    load_reuse_object_hash, publish_existing_build, record_existing_source_object,
};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinSet;

const REUSE_LOOKUP_BATCH: usize = 256;

/// Ordered distinct object hashes currently known for one graph node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectCandidates {
    hashes: Vec<ObjectHash>,
}

impl ObjectCandidates {
    fn new(hashes: impl IntoIterator<Item = ObjectHash>) -> Self {
        let mut seen = HashSet::new();
        Self {
            hashes: hashes
                .into_iter()
                .filter(|hash| seen.insert(*hash))
                .collect(),
        }
    }

    /// Returns hashes in deterministic resolution priority order.
    pub fn hashes(&self) -> &[ObjectHash] {
        &self.hashes
    }

    fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// Failure while resolving candidates, acquiring content, or building a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicRealizeError {
    message: String,
    cancelled: bool,
}

impl DynamicRealizeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
        }
    }

    fn cancelled(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: true,
        }
    }

    fn into_cancelled(mut self) -> Self {
        self.cancelled = true;
        self
    }

    /// Returns whether external or propagated cancellation caused this error.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl fmt::Display for DynamicRealizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DynamicRealizeError {}

impl From<StoreError> for DynamicRealizeError {
    fn from(error: StoreError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<BuildExecutorError> for DynamicRealizeError {
    fn from(error: BuildExecutorError) -> Self {
        Self::new(error.to_string())
    }
}

type CandidateResult = Result<ObjectCandidates, DynamicRealizeError>;
type LocalResult = Result<ObjectHash, DynamicRealizeError>;
type CandidateCell = Arc<OnceCell<CandidateResult>>;
type LocalCell = Arc<OnceCell<LocalResult>>;
type ContentCell = Arc<OnceCell<Result<bool, DynamicRealizeError>>>;
type BoxRealizeFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runtime context for hash-only DAG resolution and demand-driven realization.
pub struct DynamicRealizer {
    graph: Arc<PlannedGraph>,
    store: Store,
    run: Arc<Run>,
    logger: Arc<BuildRunLogger>,
    runtime_provider: RuntimeProvider,
    cancellation: CancellationToken,
    secondary: Arc<SecondaryResolver>,
    build_executor: BuildExecutorHandle,
    source_engine: Arc<SourceEngine>,
    candidate_cells: Mutex<HashMap<BuildKey, CandidateCell>>,
    local_cells: Mutex<HashMap<BuildKey, LocalCell>>,
    content_cells: Mutex<HashMap<ObjectHash, ContentCell>>,
    built_keys: Mutex<HashSet<BuildKey>>,
}

impl fmt::Debug for DynamicRealizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DynamicRealizer")
            .field("goals", &self.graph.goals())
            .field("store", &self.store)
            .field("run", &self.run)
            .field("runtime_provider", &self.runtime_provider)
            .field("cancellation", &self.cancellation)
            .finish_non_exhaustive()
    }
}

impl DynamicRealizer {
    /// Creates a Realizer over an already planned graph.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        graph: Arc<PlannedGraph>,
        store: Store,
        run: Arc<Run>,
        logger: Arc<BuildRunLogger>,
        runtime_provider: RuntimeProvider,
        cancellation: CancellationToken,
        secondary: Arc<SecondaryResolver>,
        build_executor: BuildExecutorHandle,
        limits: crate::acquisition::Limits,
    ) -> Result<Self, DynamicRealizeError> {
        let max_local_jobs = limits.resolved_max_local_jobs();
        if max_local_jobs == 0 {
            return Err(DynamicRealizeError::new(
                "DynamicRealizer max_local_jobs must be greater than zero",
            ));
        }
        if secondary.working().root() != store.root() {
            return Err(DynamicRealizeError::new(format!(
                "DynamicRealizer working store '{}' differs from SecondaryResolver store '{}'",
                store.root().display(),
                secondary.working().root().display()
            )));
        }
        let source_engine = engine_for_dynamic_realizer(
            store.clone(),
            run.clone(),
            logger.clone(),
            cancellation.clone(),
            secondary.clone(),
            limits,
        )
        .map_err(DynamicRealizeError::new)?;
        Ok(Self {
            graph,
            store,
            run,
            logger,
            runtime_provider,
            cancellation,
            secondary,
            build_executor,
            source_engine,
            candidate_cells: Mutex::new(HashMap::new()),
            local_cells: Mutex::new(HashMap::new()),
            content_cells: Mutex::new(HashMap::new()),
            built_keys: Mutex::new(HashSet::new()),
        })
    }

    /// Resolves all ordered goals to complete objects in the working store.
    pub async fn realize_goals(
        self: Arc<Self>,
    ) -> Result<Vec<(BuildKey, ObjectHash)>, DynamicRealizeError> {
        let cancellation_monitor = {
            let realizer = self.clone();
            tokio::spawn(async move {
                loop {
                    if realizer.cancellation.is_cancelled() {
                        realizer.source_engine.cancel();
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
        };
        let mut tasks = JoinSet::new();
        for (index, key) in self.graph.goals().iter().copied().enumerate() {
            let realizer = self.clone();
            tasks.spawn(async move { (index, key, realizer.realize_local(key).await) });
        }
        let mut results = vec![None; self.graph.goals().len()];
        while let Some(joined) = tasks.join_next().await {
            let (index, key, result) = joined.map_err(|error| {
                DynamicRealizeError::new(format!("DAG realization task panicked: {error}"))
            })?;
            match result {
                Ok(hash) => results[index] = Some((key, hash)),
                Err(error) => {
                    let cancelled = self.cancellation.is_cancelled() || error.is_cancelled();
                    self.cancellation.cancel();
                    self.source_engine.cancel();
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    cancellation_monitor.abort();
                    return Err(if cancelled {
                        error.into_cancelled()
                    } else {
                        error
                    });
                }
            }
        }
        cancellation_monitor.abort();
        Ok(results
            .into_iter()
            .map(|result| result.expect("every goal task produces one result"))
            .collect())
    }

    /// Resolves one node to ordered hashes without demanding local content.
    pub fn candidates(self: &Arc<Self>, key: BuildKey) -> BoxRealizeFuture<'_, CandidateResult> {
        Box::pin(async move {
            let cell = {
                let mut cells = self.candidate_cells.lock().await;
                cells
                    .entry(key)
                    .or_insert_with(|| Arc::new(OnceCell::new()))
                    .clone()
            };
            let mut result = cell
                .get_or_init(|| self.compute_candidates(key))
                .await
                .clone();
            if let Ok(candidates) = &mut result
                && let Some(local) = self.known_local(key).await
                && !candidates.hashes.contains(&local)
            {
                candidates.hashes.push(local);
            }
            result
        })
    }

    /// Resolves one node to complete local content, deduplicated by build key.
    pub fn realize_local(self: &Arc<Self>, key: BuildKey) -> BoxRealizeFuture<'_, LocalResult> {
        Box::pin(async move {
            let cell = {
                let mut cells = self.local_cells.lock().await;
                cells
                    .entry(key)
                    .or_insert_with(|| Arc::new(OnceCell::new()))
                    .clone()
            };
            cell.get_or_init(|| self.compute_local(key)).await.clone()
        })
    }

    async fn compute_candidates(self: &Arc<Self>, key: BuildKey) -> CandidateResult {
        self.check_cancelled()?;
        let node = self.node(key)?;
        if let Some(hash) = self.working_build(key).await? {
            return Ok(ObjectCandidates::new([hash]));
        }
        let exact = self.secondary_builds(&[key]).await?;
        if let Some(report) = exact.first() {
            self.log_mapping("build", &key.to_string(), report);
            if !report.object_hashes.is_empty() {
                return Ok(ObjectCandidates::new(report.object_hashes.iter().copied()));
            }
        }
        if let Some(source) = node.as_source() {
            return Ok(ObjectCandidates::new([source.declared_object_hash()]));
        }
        let builder = node
            .as_builder()
            .expect("planned node is Source or Builder");
        let reuse = self.cached_reuse_candidates(builder).await?;
        if !reuse.is_empty() {
            return Ok(reuse);
        }
        let built = self.build_builder(builder).await?;
        Ok(ObjectCandidates::new([built]))
    }

    async fn compute_local(self: &Arc<Self>, key: BuildKey) -> LocalResult {
        self.check_cancelled()?;
        let node = self.node(key)?;
        let candidates = self.candidates(key).await?;
        for hash in candidates.hashes().iter().copied() {
            if self.ensure_object(hash).await? {
                self.publish_cached(&node, hash).await?;
                return Ok(hash);
            }
        }
        match node.as_ref() {
            PlannedNode::Source(source) => self.materialize_source(source.clone()).await,
            PlannedNode::Builder(builder) => self.build_builder(builder).await,
        }
    }

    async fn cached_reuse_candidates(
        self: &Arc<Self>,
        builder: &BuilderPlannedSubject,
    ) -> CandidateResult {
        let slots = self.input_candidate_slots(builder).await?;
        let mut working_hashes = Vec::new();
        for inputs in CandidateProduct::new(&slots) {
            let reuse_key = builder
                .compute_reuse_key(&inputs)
                .map_err(|error| DynamicRealizeError::new(error.to_string()))?;
            if let Some(hash) = self.working_reuse(reuse_key).await? {
                working_hashes.push(hash);
            }
        }
        if !working_hashes.is_empty() {
            return Ok(ObjectCandidates::new(working_hashes));
        }

        let mut output_hashes = Vec::new();
        let mut batch = Vec::with_capacity(REUSE_LOOKUP_BATCH);
        for inputs in CandidateProduct::new(&slots) {
            let reuse_key = builder
                .compute_reuse_key(&inputs)
                .map_err(|error| DynamicRealizeError::new(error.to_string()))?;
            batch.push(reuse_key);
            if batch.len() == REUSE_LOOKUP_BATCH {
                self.append_secondary_reuse_hashes(&batch, &mut output_hashes)
                    .await?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            self.append_secondary_reuse_hashes(&batch, &mut output_hashes)
                .await?;
        }
        Ok(ObjectCandidates::new(output_hashes))
    }

    async fn append_secondary_reuse_hashes(
        &self,
        keys: &[ReuseKey],
        output: &mut Vec<ObjectHash>,
    ) -> Result<(), DynamicRealizeError> {
        for report in self.secondary_reuses(keys).await? {
            self.log_mapping("reuse", &report.key.to_string(), &report);
            output.extend(report.object_hashes);
        }
        Ok(())
    }

    async fn build_builder(self: &Arc<Self>, builder: &BuilderPlannedSubject) -> LocalResult {
        self.check_cancelled()?;
        let mut tasks = JoinSet::new();
        for (index, (name, key)) in builder.inputs().iter().enumerate() {
            let realizer = self.clone();
            let name = name.clone();
            let key = *key;
            tasks.spawn(async move { (index, name, realizer.realize_local(key).await) });
        }
        let input_hashes =
            collect_ordered_input_tasks(tasks, builder.inputs().len(), "local realization")
                .await?
                .into_iter()
                .collect::<BTreeMap<_, _>>();
        let reuse_key = builder
            .compute_reuse_key(&input_hashes)
            .map_err(|error| DynamicRealizeError::new(error.to_string()))?;

        if let Some(hash) = self.working_reuse(reuse_key).await?
            && self.ensure_object(hash).await?
        {
            self.publish_cached_builder(builder, hash).await?;
            return Ok(hash);
        }
        let secondary = self.secondary_reuses(&[reuse_key]).await?;
        if let Some(report) = secondary.first() {
            self.log_mapping("reuse", &reuse_key.to_string(), report);
            for hash in report.object_hashes.iter().copied() {
                if self.ensure_object(hash).await? {
                    self.publish_cached_builder(builder, hash).await?;
                    return Ok(hash);
                }
            }
        }

        let builder_inputs = self.prepare_builder_inputs(builder, &input_hashes).await?;
        let node = self.node(builder.build_key())?;
        let execution = BuilderExecution::new(
            builder_inputs,
            input_hashes,
            reuse_key,
            self.store.clone(),
            self.run.clone(),
            self.logger.clone(),
            self.runtime_provider.clone(),
            self.cancellation.clone(),
        );
        let job = BuilderJob::new(node, execution)?;
        let output = execute_builder_miss(&self.build_executor, job, self.store.clone()).await?;
        self.built_keys.lock().await.insert(builder.build_key());
        Ok(output.object_hash)
    }

    async fn prepare_builder_inputs(
        &self,
        builder: &BuilderPlannedSubject,
        input_hashes: &BTreeMap<String, ObjectHash>,
    ) -> Result<BuilderInputs, DynamicRealizeError> {
        self.check_cancelled()?;
        let permit = self.acquire_local_io_permit().await?;
        self.check_cancelled()?;
        let store = self.store.clone();
        let runtime = self.runtime_provider.clone();
        let planned_inputs = builder.inputs().clone();
        let hashes = input_hashes.clone();
        let graph = self.graph.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut slots = BTreeMap::new();
            for (name, key) in planned_inputs {
                let hash = hashes.get(&name).copied().ok_or_else(|| {
                    DynamicRealizeError::new(format!(
                        "realized input hash '{}' is missing for builder",
                        name
                    ))
                })?;
                let path = if name.starts_with('_') {
                    let materialization_name = graph.node(key).map(|node| node.name());
                    materialize_fs_tree_root(&runtime, store.fs_tree(), hash, materialization_name)
                        .map_err(|error| DynamicRealizeError::new(error.to_string()))?
                } else {
                    store.object_path(hash)?.ok_or_else(|| {
                        DynamicRealizeError::new(format!(
                            "realized input object '{hash}' is missing from working store"
                        ))
                    })?
                };
                slots.insert(name, path);
            }
            Ok(BuilderInputs::new(slots))
        })
        .await
        .map_err(|error| {
            DynamicRealizeError::new(format!("input materialization task panicked: {error}"))
        })?;
        // Namespace runtime calls are synchronous. Cancellation can prevent
        // them from starting, but once started they finish atomically before
        // the Realizer observes the token at this boundary.
        self.check_cancelled()?;
        result
    }

    async fn materialize_source(
        self: &Arc<Self>,
        source: crate::SourcePlannedSubject,
    ) -> LocalResult {
        if let Some(origin) = source.origin_value().cloned() {
            let outcome = process_source(
                self.source_engine.clone(),
                SourceEntry {
                    name: source.name().to_string(),
                    object_hash: source.declared_object_hash().to_string(),
                    origin: Some(origin),
                },
            )
            .await;
            return match outcome {
                SourceOutcome::Downloaded | SourceOutcome::Local => {
                    Ok(source.declared_object_hash())
                }
                SourceOutcome::CacheHit => {
                    self.log_source_cache_hit(
                        &source,
                        source.declared_object_hash(),
                        "already_present",
                    );
                    Ok(source.declared_object_hash())
                }
                SourceOutcome::Secondary => {
                    self.log_source_cache_hit(&source, source.declared_object_hash(), "secondary");
                    Ok(source.declared_object_hash())
                }
                SourceOutcome::Mismatched(mismatch) => Err(DynamicRealizeError::new(format!(
                    "source '{}' declared object '{}' but materialized '{}'",
                    mismatch.name, mismatch.declared, mismatch.actual
                ))),
                SourceOutcome::Failed { name, message } => Err(DynamicRealizeError::new(format!(
                    "source '{name}' failed: {message}"
                ))),
            };
        }
        Err(DynamicRealizeError::new(format!(
            "source '{}' has no origin and object '{}' is unavailable",
            source.name(),
            source.declared_object_hash()
        )))
    }

    async fn publish_cached(
        self: &Arc<Self>,
        node: &Arc<PlannedNode>,
        hash: ObjectHash,
    ) -> Result<(), DynamicRealizeError> {
        match node.as_ref() {
            PlannedNode::Source(source) => {
                let store = self.store.clone();
                let name = source.name().to_string();
                let run_id = self.run.run_id().to_string();
                tokio::task::spawn_blocking(move || {
                    record_existing_source_object(&store, hash, &name, &run_id)
                })
                .await
                .map_err(|error| {
                    DynamicRealizeError::new(format!("source publication task panicked: {error}"))
                })??;
                self.log_source_cache_hit(source, hash, "already_present");
            }
            PlannedNode::Builder(builder) => {
                self.publish_cached_builder(builder, hash).await?;
                if !self.built_keys.lock().await.contains(&builder.build_key()) {
                    self.log_cache_hit(node, hash, None);
                }
            }
        }
        Ok(())
    }

    fn log_source_cache_hit(
        &self,
        source: &crate::SourcePlannedSubject,
        hash: ObjectHash,
        outcome: &str,
    ) {
        let node = PlannedNode::Source(source.clone());
        self.log_cache_hit(&node, hash, Some(outcome));
    }

    fn log_cache_hit(&self, node: &PlannedNode, hash: ObjectHash, source_outcome: Option<&str>) {
        let identity = SubjectIdentity::new(node.tag(), node.name(), node.build_key().to_string());
        let mut details = serde_json::Map::new();
        if let Some(outcome) = source_outcome {
            details.insert(
                "source_outcome".to_string(),
                serde_json::Value::String(outcome.to_string()),
            );
        }
        self.logger.log_subject_event(
            &identity,
            BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::CacheHit,
                op: None,
                message: "served from cache".to_string(),
                object_hash: Some(hash),
                raw_log_path: None,
                details,
            },
        );
    }

    async fn publish_cached_builder(
        self: &Arc<Self>,
        builder: &BuilderPlannedSubject,
        hash: ObjectHash,
    ) -> Result<(), DynamicRealizeError> {
        let mut reuse_keys = Vec::new();
        if self.working_build(builder.build_key()).await? != Some(hash) {
            let exact = self.secondary_builds(&[builder.build_key()]).await?;
            let exact_hit = exact
                .first()
                .is_some_and(|report| report.object_hashes.contains(&hash));
            if !exact_hit {
                reuse_keys = self.reuse_keys_resolving_to(builder, hash).await?;
            }
        }
        let store = self.store.clone();
        let build_key = builder.build_key();
        let name = builder.name().to_string();
        let run_id = self.run.run_id().to_string();
        tokio::task::spawn_blocking(move || {
            publish_existing_build(&store, build_key, &reuse_keys, hash, &name, &run_id)
        })
        .await
        .map_err(|error| {
            DynamicRealizeError::new(format!("mapping publication task panicked: {error}"))
        })??;
        Ok(())
    }

    async fn reuse_keys_resolving_to(
        self: &Arc<Self>,
        builder: &BuilderPlannedSubject,
        wanted: ObjectHash,
    ) -> Result<Vec<ReuseKey>, DynamicRealizeError> {
        let slots = self.input_candidate_slots(builder).await?;
        let mut matching = Vec::new();
        for inputs in CandidateProduct::new(&slots) {
            let key = builder
                .compute_reuse_key(&inputs)
                .map_err(|error| DynamicRealizeError::new(error.to_string()))?;
            if self.working_reuse(key).await? == Some(wanted) {
                matching.push(key);
            }
        }

        let mut batch = Vec::with_capacity(REUSE_LOOKUP_BATCH);
        for inputs in CandidateProduct::new(&slots) {
            let key = builder
                .compute_reuse_key(&inputs)
                .map_err(|error| DynamicRealizeError::new(error.to_string()))?;
            batch.push(key);
            if batch.len() == REUSE_LOOKUP_BATCH {
                for report in self.secondary_reuses(&batch).await? {
                    if report.object_hashes.contains(&wanted) {
                        matching.push(report.key);
                    }
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            for report in self.secondary_reuses(&batch).await? {
                if report.object_hashes.contains(&wanted) {
                    matching.push(report.key);
                }
            }
        }
        Ok(dedup_reuse_keys(matching))
    }

    async fn input_candidate_slots(
        self: &Arc<Self>,
        builder: &BuilderPlannedSubject,
    ) -> Result<Vec<(String, Vec<ObjectHash>)>, DynamicRealizeError> {
        let mut tasks = JoinSet::new();
        for (index, (name, key)) in builder.inputs().iter().enumerate() {
            let realizer = self.clone();
            let name = name.clone();
            let key = *key;
            tasks.spawn(async move { (index, name, realizer.candidates(key).await) });
        }
        let candidates =
            collect_ordered_input_tasks(tasks, builder.inputs().len(), "candidate resolution")
                .await?;
        let mut slots = Vec::with_capacity(candidates.len());
        for (name, candidates) in candidates {
            if candidates.is_empty() {
                return Err(DynamicRealizeError::new(format!(
                    "input '{}' of builder '{}' has no object candidates",
                    name,
                    builder.name()
                )));
            }
            slots.push((name.clone(), candidates.hashes));
        }
        Ok(slots)
    }

    async fn ensure_object(
        self: &Arc<Self>,
        hash: ObjectHash,
    ) -> Result<bool, DynamicRealizeError> {
        let cell = {
            let mut cells = self.content_cells.lock().await;
            cells
                .entry(hash)
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let result = cell
            .get_or_init(|| async {
                self.check_cancelled()?;
                let permit = self.acquire_local_io_permit().await?;
                self.check_cancelled()?;
                let secondary = self.secondary.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    secondary.ensure_objects(&[hash])
                })
                .await
                .map_err(|error| {
                    DynamicRealizeError::new(format!("content acquisition task panicked: {error}"))
                })?
                .map_err(DynamicRealizeError::from)?;
                // A copy/hash transaction already inside synchronous store or
                // namespace code is allowed to reach its atomic publication
                // boundary. Do not publish graph mappings after cancellation.
                self.check_cancelled()?;
                Ok(result[0].outcome.is_some())
            })
            .await
            .clone();
        if matches!(result, Ok(false)) {
            let mut cells = self.content_cells.lock().await;
            if cells
                .get(&hash)
                .is_some_and(|current| Arc::ptr_eq(current, &cell))
            {
                cells.remove(&hash);
            }
        }
        result
    }

    async fn acquire_local_io_permit(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, DynamicRealizeError> {
        self.source_engine
            .acquire_local_permit()
            .await
            .map_err(|error| {
                if self.cancellation.is_cancelled() {
                    DynamicRealizeError::cancelled("build cancelled by signal")
                } else {
                    DynamicRealizeError::new(error.to_string())
                }
            })
    }

    async fn known_local(&self, key: BuildKey) -> Option<ObjectHash> {
        let cell = self.local_cells.lock().await.get(&key).cloned()?;
        cell.get().and_then(|result| result.as_ref().ok().copied())
    }

    async fn working_build(
        &self,
        key: BuildKey,
    ) -> Result<Option<ObjectHash>, DynamicRealizeError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || load_build_object_hash(&store, key))
            .await
            .map_err(|error| {
                DynamicRealizeError::new(format!("working build lookup panicked: {error}"))
            })?
            .map_err(DynamicRealizeError::from)
    }

    async fn working_reuse(
        &self,
        key: ReuseKey,
    ) -> Result<Option<ObjectHash>, DynamicRealizeError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || load_reuse_object_hash(&store, key))
            .await
            .map_err(|error| {
                DynamicRealizeError::new(format!("working reuse lookup panicked: {error}"))
            })?
            .map_err(DynamicRealizeError::from)
    }

    async fn secondary_builds(
        &self,
        keys: &[BuildKey],
    ) -> Result<Vec<MappingCandidates<BuildKey>>, DynamicRealizeError> {
        let resolver = self.secondary.clone();
        let keys = keys.to_vec();
        tokio::task::spawn_blocking(move || resolver.lookup_builds(&keys))
            .await
            .map_err(|error| {
                DynamicRealizeError::new(format!("secondary build lookup panicked: {error}"))
            })?
            .map_err(DynamicRealizeError::from)
    }

    async fn secondary_reuses(
        &self,
        keys: &[ReuseKey],
    ) -> Result<Vec<MappingCandidates<ReuseKey>>, DynamicRealizeError> {
        let resolver = self.secondary.clone();
        let keys = keys.to_vec();
        tokio::task::spawn_blocking(move || resolver.lookup_reuses(&keys))
            .await
            .map_err(|error| {
                DynamicRealizeError::new(format!("secondary reuse lookup panicked: {error}"))
            })?
            .map_err(DynamicRealizeError::from)
    }

    fn node(&self, key: BuildKey) -> Result<Arc<PlannedNode>, DynamicRealizeError> {
        self.graph.node(key).cloned().ok_or_else(|| {
            DynamicRealizeError::new(format!("planned graph is missing node '{key}'"))
        })
    }

    fn check_cancelled(&self) -> Result<(), DynamicRealizeError> {
        if self.cancellation.is_cancelled() {
            Err(DynamicRealizeError::cancelled("build cancelled by signal"))
        } else {
            Ok(())
        }
    }

    fn log_mapping<K: Copy>(&self, kind: &str, key: &str, report: &MappingCandidates<K>) {
        if report.answers.is_empty() {
            return;
        }
        let conflict = report.has_conflict();
        self.logger.log_run_event(BuildLogEvent {
            level: if conflict {
                BuildLogLevel::Warn
            } else {
                BuildLogLevel::Info
            },
            status: BuildStatus::Running,
            op: Some(format!("secondary-{kind}")),
            message: if conflict {
                format!("trusted indexes disagree for {kind} key '{key}'")
            } else {
                format!("trusted index resolved {kind} key '{key}'")
            },
            object_hash: None,
            raw_log_path: None,
            details: json!({
                "mapping_kind": kind,
                "key": key,
                "answers": report.answers.iter().map(|answer| json!({
                    "index": answer.index,
                    "object_hash": answer.object_hash,
                })).collect::<Vec<_>>(),
            })
            .as_object()
            .expect("mapping details are an object")
            .clone(),
        });
    }
}

async fn collect_ordered_input_tasks<T: Send + 'static>(
    mut tasks: JoinSet<(usize, String, Result<T, DynamicRealizeError>)>,
    count: usize,
    operation: &str,
) -> Result<Vec<(String, T)>, DynamicRealizeError> {
    let mut ordered = std::iter::repeat_with(|| None)
        .take(count)
        .collect::<Vec<Option<(String, T)>>>();
    while let Some(joined) = tasks.join_next().await {
        let (index, name, result) = match joined {
            Ok(output) => output,
            Err(error) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return Err(DynamicRealizeError::new(format!(
                    "input {operation} task panicked: {error}"
                )));
            }
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return Err(error);
            }
        };
        ordered[index] = Some((name, value));
    }
    Ok(ordered
        .into_iter()
        .map(|result| result.expect("every input task produces one result"))
        .collect())
}

struct CandidateProduct<'a> {
    slots: &'a [(String, Vec<ObjectHash>)],
    indices: Vec<usize>,
    first: bool,
    done: bool,
}

impl<'a> CandidateProduct<'a> {
    fn new(slots: &'a [(String, Vec<ObjectHash>)]) -> Self {
        Self {
            slots,
            indices: vec![0; slots.len()],
            first: true,
            done: slots.iter().any(|(_, hashes)| hashes.is_empty()),
        }
    }

    fn current(&self) -> BTreeMap<String, ObjectHash> {
        self.slots
            .iter()
            .zip(&self.indices)
            .map(|((name, hashes), index)| (name.clone(), hashes[*index]))
            .collect()
    }

    fn advance(&mut self) {
        for index in (0..self.indices.len()).rev() {
            self.indices[index] += 1;
            if self.indices[index] < self.slots[index].1.len() {
                return;
            }
            self.indices[index] = 0;
        }
        self.done = true;
    }
}

impl Iterator for CandidateProduct<'_> {
    type Item = BTreeMap<String, ObjectHash>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.first {
            self.first = false;
            return Some(self.current());
        }
        self.advance();
        (!self.done).then(|| self.current())
    }
}

fn dedup_reuse_keys(keys: Vec<ReuseKey>) -> Vec<ReuseKey> {
    let mut seen = HashSet::new();
    keys.into_iter().filter(|key| seen.insert(*key)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::plan_graph;
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use bobr_store::fs_tree::{FsFileHash, FsTreeEntry, FsTreeManifest};
    use bobr_store::{
        ContentImportOutcome, ContentSource, LocalCopyContentSource, LocalHardlinkContentSource,
        LocalRepository, LocalTrustedKeyIndex, NamedContentSource, NamedTrustedKeyIndex,
        ReadOnlyStore, import_build, import_source_object,
    };
    use serde_json::{Value, json};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::MetadataExt;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::{TempDir, tempdir};

    struct TestEnvironment {
        _temp: TempDir,
        store: Store,
        run: Arc<Run>,
        logger: Arc<BuildRunLogger>,
    }

    #[derive(Debug, Clone)]
    struct TrackedCopyContentSource {
        inner: LocalCopyContentSource,
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl TrackedCopyContentSource {
        fn new(inner: LocalCopyContentSource, delay: Duration) -> Self {
            Self {
                inner,
                calls: Arc::new(AtomicUsize::new(0)),
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    impl ContentSource for TrackedCopyContentSource {
        fn locate_objects(&self, hashes: &[ObjectHash]) -> Result<HashSet<ObjectHash>, StoreError> {
            self.inner.locate_objects(hashes)
        }

        fn object_manifest(&self, hash: ObjectHash) -> Result<Option<FsTreeManifest>, StoreError> {
            self.inner.object_manifest(hash)
        }

        fn locate_fs_files(
            &self,
            hashes: &[FsFileHash],
        ) -> Result<HashSet<FsFileHash>, StoreError> {
            self.inner.locate_fs_files(hashes)
        }

        fn import_fs_files(
            &self,
            working: &Store,
            hashes: &[FsFileHash],
        ) -> Result<(), StoreError> {
            self.inner.import_fs_files(working, hashes)
        }

        fn import_object(
            &self,
            working: &Store,
            hash: ObjectHash,
        ) -> Result<ContentImportOutcome, StoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            thread::sleep(self.delay);
            let result = self.inner.import_object(working, hash);
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    fn environment(label: &str) -> TestEnvironment {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("working");
        let logs = temp.path().join("logs");
        let work = temp.path().join("work");
        fs::create_dir(&store_root).unwrap();
        fs::create_dir(&logs).unwrap();
        fs::create_dir(&work).unwrap();
        let store = Store::create(&store_root).unwrap();
        let run = Arc::new(Run::new(format!("dynamic-{label}"), &logs, &work).unwrap());
        let logger = Arc::new(BuildRunLogger::new(&logs, run.run_id(), true).unwrap());
        TestEnvironment {
            _temp: temp,
            store,
            run,
            logger,
        }
    }

    fn store(root: &std::path::Path) -> Store {
        fs::create_dir(root).unwrap();
        Store::create(root).unwrap()
    }

    fn source(name: &str, hash: ObjectHash) -> Value {
        json!({
            "name": name,
            "tag": "Source",
            "object_hash": hash,
        })
    }

    fn path_source(name: &str, hash: ObjectHash, path: &std::path::Path) -> Value {
        json!({
            "name": name,
            "tag": "Source",
            "object_hash": hash,
            "origin": {
                "tag": "Path",
                "path": path,
                "unpack": false,
            }
        })
    }

    fn http_source(name: &str, hash: ObjectHash, url: &str) -> Value {
        json!({
            "name": name,
            "tag": "Source",
            "object_hash": hash,
            "origin": {
                "tag": "Http",
                "url": url,
                "unpack": false,
            }
        })
    }

    fn group(name: &str, input: &str) -> Value {
        json!({
            "name": name,
            "tag": "Group",
            "config": {},
            "inputs": { "input": input },
        })
    }

    fn group_with_inputs(name: &str, inputs: &[(&str, &str)]) -> Value {
        json!({
            "name": name,
            "tag": "Group",
            "config": {},
            "inputs": inputs.iter().copied().collect::<BTreeMap<_, _>>(),
        })
    }

    fn spawn_source_barrier(
        left: Vec<u8>,
        right: Vec<u8>,
    ) -> std::io::Result<(String, String, thread::JoinHandle<usize>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let left_url = format!("http://{address}/left");
        let right_url = format!("http://{address}/right");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut requests = Vec::new();
            while requests.len() < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_http_request(&mut stream);
                        requests.push((stream, request));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("failed to accept barrier request: {error}"),
                }
            }
            let concurrent = requests.len();
            for (mut stream, request) in requests {
                let body = if request.starts_with("GET /left ") {
                    Some(left.as_slice())
                } else if request.starts_with("GET /right ") {
                    Some(right.as_slice())
                } else {
                    None
                };
                let (status, body) = match (concurrent, body) {
                    (2, Some(body)) => ("HTTP/1.1 200 OK", body),
                    (2, None) => ("HTTP/1.1 404 Not Found", &[][..]),
                    _ => ("HTTP/1.1 503 Service Unavailable", &[][..]),
                };
                let response = format!(
                    "{status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
                stream.flush().unwrap();
            }
            concurrent
        });
        Ok((left_url, right_url, handle))
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
        }
        String::from_utf8(request).unwrap()
    }

    fn tree(name: &str, text: &str) -> Value {
        json!({
            "name": name,
            "tag": "Tree",
            "config": { "tree": { "entries": [{
                "type": "file",
                "path": "value",
                "text": text,
                "executable": false,
            }] } },
            "inputs": {},
        })
    }

    fn publish(
        store: &Store,
        build_key: BuildKey,
        reuse_key: ReuseKey,
        bytes: &[u8],
        staged: &std::path::Path,
    ) -> ObjectHash {
        fs::write(staged, bytes).unwrap();
        import_build(
            store,
            build_key,
            reuse_key,
            Vec::new(),
            staged,
            &format!("test-{build_key}"),
            "secondary-run",
        )
        .unwrap()
    }

    fn key(digit: char) -> BuildKey {
        BuildKey::from_str(&digit.to_string().repeat(64)).unwrap()
    }

    fn reuse(digit: char) -> ReuseKey {
        ReuseKey::from_str(&digit.to_string().repeat(64)).unwrap()
    }

    fn object_hash(digit: char) -> ObjectHash {
        ObjectHash::from_str(&digit.to_string().repeat(64)).unwrap()
    }

    fn index(name: &str, root: &std::path::Path) -> NamedTrustedKeyIndex {
        NamedTrustedKeyIndex::new(
            name,
            Arc::new(LocalTrustedKeyIndex::new(LocalRepository::new(
                ReadOnlyStore::open(root).unwrap(),
            ))),
        )
    }

    fn hardlink_content(name: &str, root: &std::path::Path) -> NamedContentSource {
        NamedContentSource::new(
            name,
            Arc::new(LocalHardlinkContentSource::with_runtime(
                LocalRepository::new(ReadOnlyStore::open(root).unwrap()),
                RuntimeProvider::host(),
            )),
        )
    }

    fn copy_content(name: &str, root: &std::path::Path) -> NamedContentSource {
        NamedContentSource::new(
            name,
            Arc::new(LocalCopyContentSource::with_runtime(
                LocalRepository::new(ReadOnlyStore::open(root).unwrap()),
                RuntimeProvider::host(),
            )),
        )
    }

    fn fs_file_path(store: &Store, hash: FsFileHash) -> std::path::PathBuf {
        let hex = hash.to_hex();
        store.root().join("fs-files").join(&hex[..2]).join(hex)
    }

    fn publish_fs_tree(
        store: &Store,
        build_key: BuildKey,
        reuse_key: ReuseKey,
        tree: &std::path::Path,
        staged: &std::path::Path,
        name: &str,
    ) -> (ObjectHash, Vec<FsFileHash>) {
        let manifest = store.fs_tree().intern_tree(tree.to_path_buf()).unwrap();
        let hashes = manifest
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .collect::<Vec<_>>();
        manifest.write_canonical(staged).unwrap();
        let object_hash = import_build(
            store,
            build_key,
            reuse_key,
            Vec::new(),
            staged,
            name,
            "secondary-run",
        )
        .unwrap();
        (object_hash, hashes)
    }

    fn publish_source_object(
        store: &Store,
        staged: &std::path::Path,
        name: &str,
        bytes: &[u8],
    ) -> ObjectHash {
        fs::write(staged, bytes).unwrap();
        let hash = fsobj_hash::hash_file_bytes(false, bytes);
        let outcome = import_source_object(store, hash, staged, name, "secondary-run").unwrap();
        assert!(matches!(
            outcome,
            bobr_store::SourceImportOutcome::Matched(_)
        ));
        hash
    }

    fn object_record_path(store: &Store, hash: ObjectHash) -> std::path::PathBuf {
        store
            .root()
            .join("object-records")
            .join(format!("{}.json", hash.to_hex()))
    }

    fn object_ref_path(store: &Store, name: &str) -> std::path::PathBuf {
        store.root().join("object-refs").join(name)
    }

    fn dynamic(
        environment: &TestEnvironment,
        graph: Arc<PlannedGraph>,
        indexes: Vec<NamedTrustedKeyIndex>,
        sources: Vec<NamedContentSource>,
    ) -> (Arc<DynamicRealizer>, crate::build_executor::BuildExecutor) {
        dynamic_with_local_jobs(
            environment,
            graph,
            indexes,
            sources,
            CancellationToken::new(),
            2,
        )
    }

    fn dynamic_with_local_jobs(
        environment: &TestEnvironment,
        graph: Arc<PlannedGraph>,
        indexes: Vec<NamedTrustedKeyIndex>,
        sources: Vec<NamedContentSource>,
        cancellation: CancellationToken,
        max_local_jobs: u32,
    ) -> (Arc<DynamicRealizer>, crate::build_executor::BuildExecutor) {
        let secondary = Arc::new(
            SecondaryResolver::new(
                environment.store.clone(),
                environment.run.run_id(),
                indexes,
                sources,
            )
            .unwrap(),
        );
        let executor = crate::build_executor::BuildExecutor::new(2, 8).unwrap();
        let realizer = Arc::new(
            DynamicRealizer::new(
                graph,
                environment.store.clone(),
                environment.run.clone(),
                environment.logger.clone(),
                RuntimeProvider::host(),
                cancellation,
                secondary,
                executor.handle(),
                crate::acquisition::Limits {
                    max_local_jobs: Some(max_local_jobs),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        (realizer, executor)
    }

    fn group_graph(input_hash: ObjectHash) -> Arc<PlannedGraph> {
        Arc::new(
            plan_graph(
                &BTreeMap::from([
                    ("input".to_string(), source("input", input_hash)),
                    ("root".to_string(), group("root", "input")),
                ]),
                &["root".to_string()],
            )
            .unwrap(),
        )
    }

    fn root_builder(graph: &PlannedGraph) -> &BuilderPlannedSubject {
        graph.node(graph.goals()[0]).unwrap().as_builder().unwrap()
    }

    #[tokio::test]
    async fn exact_mapping_and_fs_tree_copy_content_can_come_from_different_repositories() {
        let environment = environment("copy-exact-split");
        let declared = ObjectHash::from_str(&"1".repeat(64)).unwrap();
        let graph = group_graph(declared);
        let index_root = environment._temp.path().join("index");
        let content_root = environment._temp.path().join("content");
        let index_store = store(&index_root);
        let content_store = store(&content_root);
        let tree = environment._temp.path().join("exact-tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"exact fs-tree\n").unwrap();
        let (indexed_hash, indexed_files) = publish_fs_tree(
            &index_store,
            graph.goals()[0],
            reuse('1'),
            &tree,
            &environment._temp.path().join("index-manifest"),
            "indexed-exact",
        );
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"exact fs-tree\n").unwrap();
        let (content_hash, content_files) = publish_fs_tree(
            &content_store,
            key('a'),
            reuse('2'),
            &tree,
            &environment._temp.path().join("content-manifest"),
            "content-exact",
        );
        assert_eq!(indexed_hash, content_hash);
        assert_eq!(indexed_files, content_files);
        fs::remove_file(index_store.object_path(indexed_hash).unwrap().unwrap()).unwrap();
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("identity", &index_root)],
            vec![copy_content("bytes", &content_root)],
        );

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_eq!(realized, [(graph.goals()[0], content_hash)]);
        assert!(environment.store.object_is_complete(content_hash).unwrap());
        assert!(environment.store.object_path(declared).unwrap().is_none());
        assert_eq!(
            load_build_object_hash(&environment.store, graph.goals()[0]).unwrap(),
            Some(content_hash)
        );
        assert!(object_record_path(&environment.store, content_hash).is_file());
        assert!(object_ref_path(&environment.store, "root").is_symlink());
        let source_file = fs_file_path(&content_store, content_files[0]);
        let working_file = fs_file_path(&environment.store, content_files[0]);
        let source_metadata = fs::metadata(source_file).unwrap();
        let working_metadata = fs::metadata(working_file).unwrap();
        assert_eq!(source_metadata.dev(), working_metadata.dev());
        assert_ne!(source_metadata.ino(), working_metadata.ino());
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dynamic_reuse_skips_an_unavailable_candidate_and_copies_the_next_fs_tree() {
        let environment = environment("copy-reuse-fallback");
        let declared = ObjectHash::from_str(&"2".repeat(64)).unwrap();
        let graph = group_graph(declared);
        let input_key = BuildKey::from_object_hash(declared);
        let root = root_builder(&graph);
        let first_root = environment._temp.path().join("first-index");
        let second_root = environment._temp.path().join("second-index");
        let content_root = environment._temp.path().join("reuse-content");
        let first = store(&first_root);
        let second = store(&second_root);
        let content_store = store(&content_root);
        let x = publish(
            &first,
            input_key,
            reuse('3'),
            b"candidate-x\n",
            &environment._temp.path().join("candidate-x"),
        );
        let y = publish(
            &second,
            input_key,
            reuse('4'),
            b"candidate-y\n",
            &environment._temp.path().join("candidate-y"),
        );
        let rx = root
            .compute_reuse_key(&BTreeMap::from([("input".into(), x)]))
            .unwrap();
        let ry = root
            .compute_reuse_key(&BTreeMap::from([("input".into(), y)]))
            .unwrap();
        let unavailable = publish(
            &first,
            key('b'),
            rx,
            b"unavailable output\n",
            &environment._temp.path().join("unavailable-output"),
        );
        let tree = environment._temp.path().join("reuse-tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"second reuse candidate\n").unwrap();
        let (selected, _) = publish_fs_tree(
            &second,
            key('c'),
            ry,
            &tree,
            &environment._temp.path().join("selected-index-manifest"),
            "selected-index",
        );
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"second reuse candidate\n").unwrap();
        let (content_hash, _) = publish_fs_tree(
            &content_store,
            key('d'),
            reuse('5'),
            &tree,
            &environment._temp.path().join("selected-content-manifest"),
            "selected-content",
        );
        assert_eq!(selected, content_hash);
        for (repository, hash) in [
            (&first, x),
            (&first, unavailable),
            (&second, y),
            (&second, selected),
        ] {
            fs::remove_file(repository.object_path(hash).unwrap().unwrap()).unwrap();
        }
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("first", &first_root), index("second", &second_root)],
            vec![copy_content("content", &content_root)],
        );

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_eq!(realized, [(graph.goals()[0], selected)]);
        assert!(environment.store.object_is_complete(selected).unwrap());
        assert!(environment.store.object_path(x).unwrap().is_none());
        assert!(environment.store.object_path(y).unwrap().is_none());
        assert!(
            environment
                .store
                .object_path(unavailable)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            load_reuse_object_hash(&environment.store, rx).unwrap(),
            None
        );
        assert_eq!(
            load_reuse_object_hash(&environment.store, ry).unwrap(),
            Some(selected)
        );
        assert_eq!(
            load_build_object_hash(&environment.store, graph.goals()[0]).unwrap(),
            Some(selected)
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn source_uses_copy_repository_content_before_its_origin() {
        let environment = environment("copy-source-before-origin");
        let content_root = environment._temp.path().join("source-content");
        let content_store = store(&content_root);
        let tree = environment._temp.path().join("source-tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"source from repository\n").unwrap();
        let (object_hash, _) = publish_fs_tree(
            &content_store,
            key('e'),
            reuse('6'),
            &tree,
            &environment._temp.path().join("source-manifest"),
            "source-content",
        );
        let missing_origin = environment._temp.path().join("must-not-be-opened");
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([(
                    "source".to_string(),
                    path_source("source", object_hash, &missing_origin),
                )]),
                &["source".to_string()],
            )
            .unwrap(),
        );
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            Vec::new(),
            vec![copy_content("content", &content_root)],
        );

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_eq!(realized, [(graph.goals()[0], object_hash)]);
        assert!(environment.store.object_is_complete(object_hash).unwrap());
        assert!(object_ref_path(&environment.store, "source").is_symlink());
        assert!(!missing_origin.exists());
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unavailable_exact_candidate_falls_back_to_builder_execution() {
        let environment = environment("copy-build-fallback");
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([("root".to_string(), tree("root", "built value"))]),
                &["root".to_string()],
            )
            .unwrap(),
        );
        let stale_root = environment._temp.path().join("stale");
        let stale_store = store(&stale_root);
        let stale = publish(
            &stale_store,
            graph.goals()[0],
            reuse('7'),
            b"stale exact\n",
            &environment._temp.path().join("stale-object"),
        );
        fs::remove_file(stale_store.object_path(stale).unwrap().unwrap()).unwrap();
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("stale", &stale_root)],
            vec![copy_content("stale", &stale_root)],
        );

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_ne!(realized[0].1, stale);
        assert!(environment.store.object_is_complete(realized[0].1).unwrap());
        assert_eq!(
            load_build_object_hash(&environment.store, graph.goals()[0]).unwrap(),
            Some(realized[0].1)
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn incomplete_copy_candidate_publishes_no_mapping_record_or_ref() {
        let environment = environment("copy-incomplete-publication");
        let index_root = environment._temp.path().join("incomplete-index");
        let content_root = environment._temp.path().join("incomplete-content");
        let index_store = store(&index_root);
        let content_store = store(&content_root);
        let tree = environment._temp.path().join("incomplete-tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"missing closure member\n").unwrap();
        let (object_hash, hashes) = publish_fs_tree(
            &content_store,
            key('f'),
            reuse('9'),
            &tree,
            &environment._temp.path().join("incomplete-content-manifest"),
            "incomplete-content",
        );
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("payload"), b"missing closure member\n").unwrap();
        let source_key = BuildKey::from_object_hash(object_hash);
        let (indexed_hash, _) = publish_fs_tree(
            &index_store,
            source_key,
            reuse('8'),
            &tree,
            &environment._temp.path().join("incomplete-index-manifest"),
            "incomplete-index",
        );
        assert_eq!(object_hash, indexed_hash);
        fs::remove_file(index_store.object_path(object_hash).unwrap().unwrap()).unwrap();
        fs::remove_file(fs_file_path(&content_store, hashes[0])).unwrap();
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([("source".to_string(), source("source", object_hash))]),
                &["source".to_string()],
            )
            .unwrap(),
        );
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("identity", &index_root)],
            vec![copy_content("incomplete", &content_root)],
        );

        let error = realizer.clone().realize_goals().await.unwrap_err();

        assert!(error.to_string().contains("has no origin"), "{error}");
        assert!(
            environment
                .store
                .object_path(object_hash)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            load_build_object_hash(&environment.store, source_key).unwrap(),
            None
        );
        assert!(!object_record_path(&environment.store, object_hash).exists());
        assert!(!object_ref_path(&environment.store, "source").exists());
        executor.shutdown().await.unwrap();
    }

    #[test]
    fn candidate_product_is_deterministic_and_handles_an_empty_builder() {
        let a = ObjectHash::from_str(&"1".repeat(64)).unwrap();
        let b = ObjectHash::from_str(&"2".repeat(64)).unwrap();
        let c = ObjectHash::from_str(&"3".repeat(64)).unwrap();
        let slots = vec![("a".to_string(), vec![a, b]), ("b".to_string(), vec![c, a])];
        let combinations = CandidateProduct::new(&slots).collect::<Vec<_>>();
        assert_eq!(combinations.len(), 4);
        assert_eq!(
            combinations[0],
            BTreeMap::from([("a".into(), a), ("b".into(), c)])
        );
        assert_eq!(
            combinations[1],
            BTreeMap::from([("a".into(), a), ("b".into(), a)])
        );
        assert_eq!(
            combinations[2],
            BTreeMap::from([("a".into(), b), ("b".into(), c)])
        );
        assert_eq!(
            combinations[3],
            BTreeMap::from([("a".into(), b), ("b".into(), a)])
        );
        assert_eq!(
            CandidateProduct::new(&[]).collect::<Vec<_>>(),
            [BTreeMap::new()]
        );
    }

    #[tokio::test]
    async fn candidate_resolution_uses_all_input_hashes_without_importing_content() {
        let environment = environment("hash-only");
        let declared = ObjectHash::from_str(&"1".repeat(64)).unwrap();
        let graph = group_graph(declared);
        let input_key = BuildKey::from_object_hash(declared);
        let root = root_builder(&graph);
        let first_root = environment._temp.path().join("first-index");
        let second_root = environment._temp.path().join("second-index");
        let first = store(&first_root);
        let second = store(&second_root);
        let x = publish(
            &first,
            input_key,
            reuse('1'),
            b"input-x\n",
            &environment._temp.path().join("input-x"),
        );
        let y = publish(
            &second,
            input_key,
            reuse('2'),
            b"input-y\n",
            &environment._temp.path().join("input-y"),
        );
        let rx = root
            .compute_reuse_key(&BTreeMap::from([("input".to_string(), x)]))
            .unwrap();
        let ry = root
            .compute_reuse_key(&BTreeMap::from([("input".to_string(), y)]))
            .unwrap();
        let p = publish(
            &first,
            key('a'),
            rx,
            b"output-p\n",
            &environment._temp.path().join("output-p"),
        );
        let q = publish(
            &second,
            key('b'),
            ry,
            b"output-q\n",
            &environment._temp.path().join("output-q"),
        );
        for (store, hash) in [(&first, x), (&first, p), (&second, y), (&second, q)] {
            fs::remove_file(store.object_path(hash).unwrap().unwrap()).unwrap();
        }
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("first", &first_root), index("second", &second_root)],
            Vec::new(),
        );

        let candidates = realizer.candidates(graph.goals()[0]).await.unwrap();

        assert_eq!(candidates.hashes(), &[p, q]);
        for hash in [x, y, p, q] {
            assert!(environment.store.object_path(hash).unwrap().is_none());
        }
        environment.logger.flush();
        let events = fs::read_to_string(environment.run.logs_dir().join("events.jsonl")).unwrap();
        assert!(events.contains("trusted indexes disagree for build key"));
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn working_reuse_across_any_combination_beats_secondary_reuse() {
        let environment = environment("working-priority");
        let declared = ObjectHash::from_str(&"2".repeat(64)).unwrap();
        let graph = group_graph(declared);
        let input_key = BuildKey::from_object_hash(declared);
        let root = root_builder(&graph);
        let first_root = environment._temp.path().join("first-index");
        let second_root = environment._temp.path().join("second-index");
        let first = store(&first_root);
        let second = store(&second_root);
        let x = publish(
            &first,
            input_key,
            reuse('1'),
            b"x\n",
            &environment._temp.path().join("x"),
        );
        let y = publish(
            &second,
            input_key,
            reuse('2'),
            b"y\n",
            &environment._temp.path().join("y"),
        );
        let rx = root
            .compute_reuse_key(&BTreeMap::from([("input".into(), x)]))
            .unwrap();
        let ry = root
            .compute_reuse_key(&BTreeMap::from([("input".into(), y)]))
            .unwrap();
        let secondary_output = publish(
            &first,
            key('c'),
            rx,
            b"secondary\n",
            &environment._temp.path().join("secondary-output"),
        );
        let working_output = publish(
            &environment.store,
            key('d'),
            ry,
            b"working\n",
            &environment._temp.path().join("working-output"),
        );
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("first", &first_root), index("second", &second_root)],
            Vec::new(),
        );

        let candidates = realizer.candidates(graph.goals()[0]).await.unwrap();

        assert_eq!(candidates.hashes(), &[working_output]);
        assert_ne!(working_output, secondary_output);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reuse_chain_imports_only_the_goal_and_publishes_every_matching_key() {
        let environment = environment("reuse-chain");
        let declared = ObjectHash::from_str(&"3".repeat(64)).unwrap();
        let graph = group_graph(declared);
        let input_key = BuildKey::from_object_hash(declared);
        let root = root_builder(&graph);
        let first_root = environment._temp.path().join("first-index");
        let second_root = environment._temp.path().join("second-index");
        let content_root = environment._temp.path().join("content");
        let first = store(&first_root);
        let second = store(&second_root);
        let content_store = store(&content_root);
        let x = publish(
            &first,
            input_key,
            reuse('1'),
            b"x\n",
            &environment._temp.path().join("x"),
        );
        let y = publish(
            &second,
            input_key,
            reuse('2'),
            b"y\n",
            &environment._temp.path().join("y"),
        );
        let rx = root
            .compute_reuse_key(&BTreeMap::from([("input".into(), x)]))
            .unwrap();
        let ry = root
            .compute_reuse_key(&BTreeMap::from([("input".into(), y)]))
            .unwrap();
        let p = publish(
            &first,
            key('e'),
            rx,
            b"shared-output\n",
            &environment._temp.path().join("p-first"),
        );
        let same_p = publish(
            &second,
            key('f'),
            ry,
            b"shared-output\n",
            &environment._temp.path().join("p-second"),
        );
        let content_p = publish(
            &content_store,
            key('9'),
            reuse('9'),
            b"shared-output\n",
            &environment._temp.path().join("p-content"),
        );
        assert_eq!(p, same_p);
        assert_eq!(p, content_p);
        for (store, hash) in [(&first, x), (&first, p), (&second, y), (&second, p)] {
            fs::remove_file(store.object_path(hash).unwrap().unwrap()).unwrap();
        }
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("first", &first_root), index("second", &second_root)],
            vec![hardlink_content("content", &content_root)],
        );

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_eq!(realized, [(graph.goals()[0], p)]);
        assert!(environment.store.object_path(p).unwrap().is_some());
        assert!(environment.store.object_path(x).unwrap().is_none());
        assert!(environment.store.object_path(y).unwrap().is_none());
        assert_eq!(
            load_reuse_object_hash(&environment.store, rx).unwrap(),
            Some(p)
        );
        assert_eq!(
            load_reuse_object_hash(&environment.store, ry).unwrap(),
            Some(p)
        );
        assert_eq!(
            load_build_object_hash(&environment.store, graph.goals()[0]).unwrap(),
            Some(p)
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unavailable_exact_input_is_built_then_opens_a_new_parent_reuse_key() {
        let environment = environment("new-input-hash");
        let source_path = environment._temp.path().join("source.txt");
        fs::write(&source_path, b"actual-source\n").unwrap();
        let actual = fsobj_hash::hash_path(&source_path).unwrap();
        let declared = actual;
        let nodes = BTreeMap::from([
            (
                "input".to_string(),
                path_source("input", declared, &source_path),
            ),
            ("root".to_string(), group("root", "input")),
        ]);
        let graph = Arc::new(plan_graph(&nodes, &["root".to_string()]).unwrap());
        let root = root_builder(&graph);
        let index_root = environment._temp.path().join("index");
        let content_root = environment._temp.path().join("content");
        let index_store = store(&index_root);
        let content_store = store(&content_root);
        let unavailable = publish(
            &index_store,
            BuildKey::from_object_hash(declared),
            reuse('1'),
            b"unavailable-candidate\n",
            &environment._temp.path().join("unavailable"),
        );
        fs::remove_file(index_store.object_path(unavailable).unwrap().unwrap()).unwrap();
        let actual_reuse = root
            .compute_reuse_key(&BTreeMap::from([("input".into(), actual)]))
            .unwrap();
        let cached = publish(
            &index_store,
            key('7'),
            actual_reuse,
            b"parent-cached\n",
            &environment._temp.path().join("parent-index"),
        );
        fs::remove_file(index_store.object_path(cached).unwrap().unwrap()).unwrap();
        let cached_content = publish(
            &content_store,
            key('8'),
            reuse('8'),
            b"parent-cached\n",
            &environment._temp.path().join("parent-content"),
        );
        assert_eq!(cached, cached_content);
        let (realizer, executor) = dynamic(
            &environment,
            graph.clone(),
            vec![index("index", &index_root)],
            vec![copy_content("content", &content_root)],
        );

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_eq!(realized, [(graph.goals()[0], cached)]);
        assert!(environment.store.object_path(actual).unwrap().is_some());
        assert!(
            environment
                .store
                .object_path(unavailable)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            load_reuse_object_hash(&environment.store, actual_reuse).unwrap(),
            Some(cached)
        );
        environment.logger.flush();
        let events = fs::read_to_string(environment.run.logs_dir().join("events.jsonl")).unwrap();
        assert!(!events.contains("content-unavailable"));
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cold_source_content_miss_is_silent_before_materialization() {
        let environment = environment("cold-source");
        let source_path = environment._temp.path().join("source.txt");
        fs::write(&source_path, b"cold-source\n").unwrap();
        let hash = fsobj_hash::hash_path(&source_path).unwrap();
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([("root".to_string(), path_source("root", hash, &source_path))]),
                &["root".to_string()],
            )
            .unwrap(),
        );
        let (realizer, executor) = dynamic(&environment, graph.clone(), Vec::new(), Vec::new());

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_eq!(realized, [(graph.goals()[0], hash)]);
        assert!(environment.store.object_path(hash).unwrap().is_some());
        environment.logger.flush();
        let events = fs::read_to_string(environment.run.logs_dir().join("events.jsonl")).unwrap();
        assert!(!events.contains("content-unavailable"));
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_goals_build_one_shared_node() {
        let environment = environment("shared-build");
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([
                    ("first".to_string(), tree("same", "value")),
                    ("second".to_string(), tree("same", "value")),
                ]),
                &["first".to_string(), "second".to_string()],
            )
            .unwrap(),
        );
        let (realizer, executor) = dynamic(&environment, graph, Vec::new(), Vec::new());

        let realized = realizer.realize_goals().await.unwrap();

        assert_eq!(realized.len(), 2);
        assert_eq!(realized[0].1, realized[1].1);
        let workspace_count = fs::read_dir(environment.run.logs_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count();
        assert_eq!(workspace_count, 1);
        assert_eq!(environment.logger.outcome_stats().built, 1);
        assert_eq!(environment.logger.outcome_stats().cache_hit, 0);
        executor.shutdown().await.unwrap();
    }

    async fn assert_single_goal_reaches_two_sources_concurrently(nested: bool) {
        let environment = environment(if nested {
            "parallel-candidates"
        } else {
            "parallel-local"
        });
        let left = b"left source\n".to_vec();
        let right = b"right source\n".to_vec();
        let left_hash = fsobj_hash::hash_file_bytes(false, &left);
        let right_hash = fsobj_hash::hash_file_bytes(false, &right);
        let (left_url, right_url, barrier) = match spawn_source_barrier(left, right) {
            Ok(barrier) => barrier,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start source barrier: {error}"),
        };
        let mut nodes = BTreeMap::from([
            (
                "left".to_string(),
                http_source("left-source", left_hash, &left_url),
            ),
            (
                "right".to_string(),
                http_source("right-source", right_hash, &right_url),
            ),
        ]);
        if nested {
            nodes.insert("left-child".to_string(), group("left-child", "left"));
            nodes.insert("right-child".to_string(), group("right-child", "right"));
            nodes.insert(
                "root".to_string(),
                group_with_inputs("root", &[("left", "left-child"), ("right", "right-child")]),
            );
        } else {
            nodes.insert(
                "root".to_string(),
                group_with_inputs("root", &[("left", "left"), ("right", "right")]),
            );
        }
        let graph = Arc::new(plan_graph(&nodes, &["root".to_string()]).unwrap());
        let (realizer, executor) = dynamic(&environment, graph, Vec::new(), Vec::new());

        let outcome =
            tokio::time::timeout(Duration::from_secs(5), realizer.clone().realize_goals()).await;
        if outcome.is_err() {
            realizer.cancellation.cancel();
        }
        let concurrent = barrier.join().unwrap();
        executor.shutdown().await.unwrap();
        let realized = outcome
            .expect("independent Source inputs were serialized")
            .expect("single-goal realization failed");
        assert_eq!(realized.len(), 1);
        assert_eq!(concurrent, 2);
        assert_eq!(environment.logger.outcome_stats().downloaded, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repository_imports_share_the_local_io_limit_without_blocking_tokio() {
        let environment = environment("repository-local-limit");
        let repository_root = environment._temp.path().join("repository");
        let repository = store(&repository_root);
        let mut nodes = BTreeMap::new();
        let mut goals = Vec::new();
        for index in 0..4 {
            let node_name = format!("source-{index}");
            let staged = environment._temp.path().join(format!("staged-{index}"));
            let hash = publish_source_object(
                &repository,
                &staged,
                &node_name,
                format!("repository object {index}\n").as_bytes(),
            );
            nodes.insert(node_name.clone(), source(&node_name, hash));
            goals.push(node_name);
        }
        let graph = Arc::new(plan_graph(&nodes, &goals).unwrap());
        let tracked = TrackedCopyContentSource::new(
            LocalCopyContentSource::with_runtime(
                LocalRepository::new(ReadOnlyStore::open(&repository_root).unwrap()),
                RuntimeProvider::host(),
            ),
            Duration::from_millis(75),
        );
        let (realizer, executor) = dynamic_with_local_jobs(
            &environment,
            graph,
            Vec::new(),
            vec![NamedContentSource::new(
                "tracked",
                Arc::new(tracked.clone()),
            )],
            CancellationToken::new(),
            2,
        );

        let task = tokio::spawn(realizer.realize_goals());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !task.is_finished(),
            "repository imports unexpectedly blocked the Tokio runtime"
        );
        let realized = task.await.unwrap().unwrap();
        executor.shutdown().await.unwrap();

        assert_eq!(realized.len(), 4);
        assert_eq!(tracked.calls(), 4);
        assert_eq!(tracked.peak(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn object_hash_once_cell_deduplicates_repository_import() {
        let environment = environment("repository-once-cell");
        let repository_root = environment._temp.path().join("repository");
        let repository = store(&repository_root);
        let hash = publish_source_object(
            &repository,
            &environment._temp.path().join("staged"),
            "shared-source",
            b"shared repository object\n",
        );
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([("source".to_string(), source("source", hash))]),
                &["source".to_string()],
            )
            .unwrap(),
        );
        let tracked = TrackedCopyContentSource::new(
            LocalCopyContentSource::with_runtime(
                LocalRepository::new(ReadOnlyStore::open(&repository_root).unwrap()),
                RuntimeProvider::host(),
            ),
            Duration::from_millis(50),
        );
        let (realizer, executor) = dynamic(
            &environment,
            graph,
            Vec::new(),
            vec![NamedContentSource::new(
                "tracked",
                Arc::new(tracked.clone()),
            )],
        );

        let (left, middle, right) = tokio::join!(
            realizer.ensure_object(hash),
            realizer.ensure_object(hash),
            realizer.ensure_object(hash),
        );
        executor.shutdown().await.unwrap();

        assert_eq!(
            (left.unwrap(), middle.unwrap(), right.unwrap()),
            (true, true, true)
        );
        assert_eq!(tracked.calls(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_interrupts_a_repository_import_waiting_for_local_io() {
        let environment = environment("repository-cancel-wait");
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([("source".to_string(), source("source", object_hash('a')))]),
                &["source".to_string()],
            )
            .unwrap(),
        );
        let cancellation = CancellationToken::new();
        let (realizer, executor) = dynamic_with_local_jobs(
            &environment,
            graph,
            Vec::new(),
            Vec::new(),
            cancellation.clone(),
            1,
        );
        let permit = realizer.source_engine.acquire_local_permit().await.unwrap();
        let task = {
            let realizer = realizer.clone();
            tokio::spawn(async move { realizer.ensure_object(object_hash('a')).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancellation.cancel();
        realizer.source_engine.cancel();

        let error = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled local-I/O wait did not wake")
            .unwrap()
            .unwrap_err();
        drop(permit);
        executor.shutdown().await.unwrap();
        assert!(error.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_after_copy_start_leaves_only_atomic_content() {
        let environment = environment("repository-cancel-active");
        let repository_root = environment._temp.path().join("repository");
        let repository = store(&repository_root);
        let hash = publish_source_object(
            &repository,
            &environment._temp.path().join("staged"),
            "active-source",
            b"active repository object\n",
        );
        let graph = Arc::new(
            plan_graph(
                &BTreeMap::from([("source".to_string(), source("source", hash))]),
                &["source".to_string()],
            )
            .unwrap(),
        );
        let tracked = TrackedCopyContentSource::new(
            LocalCopyContentSource::with_runtime(
                LocalRepository::new(ReadOnlyStore::open(&repository_root).unwrap()),
                RuntimeProvider::host(),
            ),
            Duration::from_millis(100),
        );
        let cancellation = CancellationToken::new();
        let (realizer, executor) = dynamic_with_local_jobs(
            &environment,
            graph,
            Vec::new(),
            vec![NamedContentSource::new(
                "tracked",
                Arc::new(tracked.clone()),
            )],
            cancellation.clone(),
            1,
        );
        let task = {
            let realizer = realizer.clone();
            tokio::spawn(async move { realizer.ensure_object(hash).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while tracked.calls() == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("repository import did not start");
        cancellation.cancel();
        realizer.source_engine.cancel();

        let error = task.await.unwrap().unwrap_err();
        executor.shutdown().await.unwrap();
        assert!(error.is_cancelled());
        assert!(environment.store.object_is_complete(hash).unwrap());
        assert!(
            fs::read_dir(environment.store.root())
                .unwrap()
                .all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".bobr-repository-")
                })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_goal_realizes_independent_inputs_concurrently() {
        assert_single_goal_reaches_two_sources_concurrently(false).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_goal_resolves_independent_builder_candidates_concurrently() {
        assert_single_goal_reaches_two_sources_concurrently(true).await;
    }
}
