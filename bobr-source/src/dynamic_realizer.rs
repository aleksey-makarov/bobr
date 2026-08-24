//! Demand-driven candidate resolution and dynamic reuse for a planned DAG.
//!
//! A node first resolves to an ordered set of object hashes. Trusted mappings
//! are not accompanied by content lookup, so these hashes can flow into parent
//! reuse keys without entering the working store. Content is acquired only for
//! goals and for inputs of a builder that reached a complete reuse miss.

use crate::build_executor::{
    BuildExecutorError, BuildExecutorHandle, BuilderExecution, BuilderJob,
};
use crate::fetch::SourceEntry;
use crate::fetch::engine::{
    Engine as SourceEngine, SourceOutcome, engine_for_dynamic_realizer, process_source,
};
use crate::graph::{PlannedGraph, PlannedNode};
use crate::realizer::execute_builder_miss;
use bobr_builder::{BuilderInputs, BuilderPlannedSubject, materialize_fs_tree_root};
use bobr_core::{
    BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, BuildSeed, BuildStatus,
    CancellationToken, ObjectHash, ReuseKey, Run, RuntimeProvider, SubjectRunContext,
};
use bobr_store::{
    MappingCandidates, SecondaryResolver, SourceImportOutcome, Store, StoreError,
    import_source_object, load_build_handle, load_reuse_handle, publish_existing_build,
    record_existing_source_object,
};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, Semaphore};
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
}

impl DynamicRealizeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
    local_io: Arc<Semaphore>,
    candidate_cells: Mutex<HashMap<BuildKey, CandidateCell>>,
    local_cells: Mutex<HashMap<BuildKey, LocalCell>>,
    content_cells: Mutex<HashMap<ObjectHash, ContentCell>>,
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
        max_local_jobs: usize,
    ) -> Result<Self, DynamicRealizeError> {
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
            max_local_jobs,
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
            local_io: Arc::new(Semaphore::new(max_local_jobs)),
            candidate_cells: Mutex::new(HashMap::new()),
            local_cells: Mutex::new(HashMap::new()),
            content_cells: Mutex::new(HashMap::new()),
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
                    self.cancellation.cancel();
                    self.source_engine.cancel();
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    cancellation_monitor.abort();
                    return Err(error);
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
        let mut input_hashes = BTreeMap::new();
        for (name, key) in builder.inputs() {
            input_hashes.insert(name.clone(), self.realize_local(*key).await?);
        }
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
        Ok(
            execute_builder_miss(&self.build_executor, job, self.store.clone())
                .await?
                .object_hash,
        )
    }

    async fn prepare_builder_inputs(
        &self,
        builder: &BuilderPlannedSubject,
        input_hashes: &BTreeMap<String, ObjectHash>,
    ) -> Result<BuilderInputs, DynamicRealizeError> {
        let permit = self
            .local_io
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| DynamicRealizeError::new("local-I/O semaphore closed"))?;
        let store = self.store.clone();
        let runtime = self.runtime_provider.clone();
        let planned_inputs = builder.inputs().clone();
        let hashes = input_hashes.clone();
        let graph = self.graph.clone();
        tokio::task::spawn_blocking(move || {
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
        })?
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
                SourceOutcome::Downloaded
                | SourceOutcome::CacheHit
                | SourceOutcome::Local
                | SourceOutcome::Secondary => Ok(source.declared_object_hash()),
                SourceOutcome::Mismatched(mismatch) => Err(DynamicRealizeError::new(format!(
                    "source '{}' declared object '{}' but materialized '{}'",
                    mismatch.name, mismatch.declared, mismatch.actual
                ))),
                SourceOutcome::Failed { name, message } => Err(DynamicRealizeError::new(format!(
                    "source '{name}' failed: {message}"
                ))),
            };
        }
        if source.origin().is_none() {
            return Err(DynamicRealizeError::new(format!(
                "source '{}' has no origin and object '{}' is unavailable",
                source.name(),
                source.declared_object_hash()
            )));
        }
        let permit = self
            .local_io
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| DynamicRealizeError::new("local-I/O semaphore closed"))?;
        let store = self.store.clone();
        let run = self.run.clone();
        let run_logger = self.logger.clone();
        let runtime = self.runtime_provider.clone();
        let cancellation = self.cancellation.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if cancellation.is_cancelled() {
                return Err(DynamicRealizeError::new("build cancelled by signal"));
            }
            let workspace = run
                .create_workspace("Source", source.name(), source.build_key().to_string())
                .map_err(|error| DynamicRealizeError::new(error.to_string()))?;
            let mut scratch =
                SourceScratchGuard::new(run.clone(), workspace.temp_dir().to_path_buf());
            let logger = run_logger
                .bind_subject(source.log_subject(&workspace))
                .map_err(DynamicRealizeError::new)?;
            scratch.set_logger(logger.clone());
            run.prepare_scratch(workspace.temp_dir())
                .map_err(|error| DynamicRealizeError::new(error.to_string()))?;
            let context = SubjectRunContext::new(
                workspace,
                logger,
                cancellation.clone(),
                runtime,
                BuildSeed::ZERO,
            );
            let staged = source
                .execute(&context)
                .map_err(|error| DynamicRealizeError::new(error.to_string()))?;
            if cancellation.is_cancelled() {
                return Err(DynamicRealizeError::new("build cancelled by signal"));
            }
            match import_source_object(
                &store,
                source.declared_object_hash(),
                &staged,
                source.name(),
                run.run_id(),
            )? {
                SourceImportOutcome::Matched(hash) => Ok(hash),
                SourceImportOutcome::Mismatched { actual_hash } => {
                    Err(DynamicRealizeError::new(format!(
                        "source '{}' declared object '{}' but materialized '{}'",
                        source.name(),
                        source.declared_object_hash(),
                        actual_hash
                    )))
                }
            }
        })
        .await
        .map_err(|error| {
            DynamicRealizeError::new(format!("source materialization task panicked: {error}"))
        })?
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
                Ok(())
            }
            PlannedNode::Builder(builder) => self.publish_cached_builder(builder, hash).await,
        }
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
        let mut slots = Vec::new();
        for (name, key) in builder.inputs() {
            let candidates = self.candidates(*key).await?;
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
                let secondary = self.secondary.clone();
                tokio::task::spawn_blocking(move || secondary.ensure_objects(&[hash]))
                    .await
                    .map_err(|error| {
                        DynamicRealizeError::new(format!(
                            "content acquisition task panicked: {error}"
                        ))
                    })?
                    .map_err(DynamicRealizeError::from)
                    .map(|reports| reports[0].outcome.is_some())
            })
            .await
            .clone();
        if matches!(result, Ok(false)) {
            self.logger.log_run_event(BuildLogEvent {
                level: BuildLogLevel::Warn,
                status: BuildStatus::CacheMiss,
                op: Some("content-unavailable".to_string()),
                message: format!("object '{hash}' is unavailable from configured content sources"),
                object_hash: Some(hash),
                raw_log_path: None,
                details: serde_json::Map::new(),
            });
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

    async fn known_local(&self, key: BuildKey) -> Option<ObjectHash> {
        let cell = self.local_cells.lock().await.get(&key).cloned()?;
        cell.get().and_then(|result| result.as_ref().ok().copied())
    }

    async fn working_build(
        &self,
        key: BuildKey,
    ) -> Result<Option<ObjectHash>, DynamicRealizeError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || load_build_handle(&store, key))
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
        tokio::task::spawn_blocking(move || load_reuse_handle(&store, key))
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
            Err(DynamicRealizeError::new("build cancelled by signal"))
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

struct SourceScratchGuard {
    run: Arc<Run>,
    path: PathBuf,
    logger: Option<Arc<dyn BuildLogger>>,
}

impl SourceScratchGuard {
    fn new(run: Arc<Run>, path: PathBuf) -> Self {
        Self {
            run,
            path,
            logger: None,
        }
    }

    fn set_logger(&mut self, logger: Arc<dyn BuildLogger>) {
        self.logger = Some(logger);
    }
}

impl Drop for SourceScratchGuard {
    fn drop(&mut self) {
        if let Err(error) = self.run.remove_scratch(&self.path)
            && let Some(logger) = &self.logger
        {
            logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Warn,
                status: BuildStatus::Cleanup,
                op: Some("cleanup".to_string()),
                message: format!(
                    "failed to remove temp dir '{}': {error}",
                    self.path.display()
                ),
                object_hash: None,
                raw_log_path: None,
                details: serde_json::Map::new(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::plan_graph;
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use bobr_store::{
        LocalHardlinkContentSource, LocalTrustedKeyIndex, NamedContentSource, NamedTrustedKeyIndex,
        ReadOnlyStore, import_build,
    };
    use serde_json::{Value, json};
    use std::fs;
    use std::str::FromStr;
    use tempfile::{TempDir, tempdir};

    struct TestEnvironment {
        _temp: TempDir,
        store: Store,
        run: Arc<Run>,
        logger: Arc<BuildRunLogger>,
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

    fn group(name: &str, input: &str) -> Value {
        json!({
            "name": name,
            "tag": "Group",
            "config": {},
            "inputs": { "input": input },
        })
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

    fn index(name: &str, root: &std::path::Path) -> NamedTrustedKeyIndex {
        NamedTrustedKeyIndex::new(
            name,
            Arc::new(LocalTrustedKeyIndex::new(
                ReadOnlyStore::open(root).unwrap(),
            )),
        )
    }

    fn content(name: &str, root: &std::path::Path) -> NamedContentSource {
        NamedContentSource::new(
            name,
            Arc::new(LocalHardlinkContentSource::with_runtime(
                ReadOnlyStore::open(root).unwrap(),
                RuntimeProvider::host(),
            )),
        )
    }

    fn dynamic(
        environment: &TestEnvironment,
        graph: Arc<PlannedGraph>,
        indexes: Vec<NamedTrustedKeyIndex>,
        sources: Vec<NamedContentSource>,
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
                CancellationToken::new(),
                secondary,
                executor.handle(),
                2,
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
            vec![content("content", &content_root)],
        );

        let realized = realizer.clone().realize_goals().await.unwrap();

        assert_eq!(realized, [(graph.goals()[0], p)]);
        assert!(environment.store.object_path(p).unwrap().is_some());
        assert!(environment.store.object_path(x).unwrap().is_none());
        assert!(environment.store.object_path(y).unwrap().is_none());
        assert_eq!(load_reuse_handle(&environment.store, rx).unwrap(), Some(p));
        assert_eq!(load_reuse_handle(&environment.store, ry).unwrap(), Some(p));
        assert_eq!(
            load_build_handle(&environment.store, graph.goals()[0]).unwrap(),
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
            vec![content("content", &content_root)],
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
            load_reuse_handle(&environment.store, actual_reuse).unwrap(),
            Some(cached)
        );
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
        executor.shutdown().await.unwrap();
    }
}
