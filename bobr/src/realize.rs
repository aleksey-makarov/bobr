use crate::execution::ExecutionError;
use crate::request::Request;
use bobr_core::{
    BuildLogEvent, BuildLogLevel, BuildRunLogger, BuildStatus, CancellationToken, ObjectHash, Run,
};
use bobr_runtime::runtime_provider::runtime_provider_for_current_process;
use bobr_source::build_executor::BuildExecutor;
use bobr_source::dynamic_realizer::DynamicRealizer;
use bobr_source::graph::{GraphPlanError, GraphPlanErrorKind, plan_graph};
use bobr_store::{
    LocalHardlinkContentSource, LocalTrustedKeyIndex, NamedContentSource, NamedTrustedKeyIndex,
    ReadOnlyStore, SecondaryResolver, Store,
};
use serde::Serialize;
use serde_json::json;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::thread;

/// One ordered goal result returned by the unified Realizer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalResult {
    /// Request node ID named in `goals`.
    pub node: String,
    /// Complete object published in the working store.
    pub object_hash: ObjectHash,
}

/// Executes the unified multi-goal request through [`DynamicRealizer`].
pub async fn realize(
    request: Request,
    cancellation: CancellationToken,
) -> Result<Vec<GoalResult>, ExecutionError> {
    let Request {
        store: store_path,
        logs,
        work,
        run_id,
        quiet,
        jobs,
        progress,
        limits,
        secondaries,
        goals,
        nodes,
        ..
    } = request;
    let jobs = jobs.unwrap_or_else(default_jobs);
    let graph = Arc::new(plan_graph(&nodes, &goals).map_err(map_graph_error)?);
    let reachable = graph.nodes().len();
    let reachable_sources = graph
        .nodes()
        .values()
        .filter(|node| node.as_source().is_some())
        .count();
    let reachable_builders = reachable - reachable_sources;
    let store = Store::create(&store_path).map_err(map_store_error)?;
    let run = Arc::new(Run::new(run_id, &logs, &work)?);
    check_same_filesystem(&store, &run)?;
    let logger = Arc::new(
        BuildRunLogger::new_with_progress(
            run.logs_dir(),
            run.run_id(),
            quiet.unwrap_or(false),
            progress,
        )
        .map_err(ExecutionError::Store)?,
    );
    let _resize_monitor = ResizeMonitor::spawn(&logger);
    let runtime_provider = runtime_provider_for_current_process();
    let indexes = secondaries
        .trusted_indexes
        .into_iter()
        .map(|entry| {
            let read_only = ReadOnlyStore::open(&entry.store).map_err(map_store_error)?;
            Ok(NamedTrustedKeyIndex::new(
                entry.name,
                Arc::new(LocalTrustedKeyIndex::new(read_only)),
            ))
        })
        .collect::<Result<Vec<_>, ExecutionError>>()?;
    let sources = secondaries
        .content_sources
        .into_iter()
        .map(|entry| {
            let read_only = ReadOnlyStore::open(&entry.store).map_err(map_store_error)?;
            check_content_source_filesystem(&store, &read_only, &entry.name)?;
            Ok(NamedContentSource::new(
                entry.name,
                Arc::new(LocalHardlinkContentSource::with_runtime(
                    read_only,
                    runtime_provider.clone(),
                )),
            ))
        })
        .collect::<Result<Vec<_>, ExecutionError>>()?;
    let secondary = Arc::new(
        SecondaryResolver::new(store.clone(), run.run_id(), indexes, sources)
            .map_err(map_store_error)?,
    );
    let queue_capacity = jobs.saturating_mul(2).max(1);
    let executor = BuildExecutor::new(jobs, queue_capacity)
        .map_err(|error| ExecutionError::Build(error.to_string()))?;
    let dynamic = Arc::new(
        DynamicRealizer::new(
            graph,
            store,
            run.clone(),
            logger.clone(),
            runtime_provider,
            cancellation.clone(),
            secondary,
            executor.handle(),
            limits,
        )
        .map_err(|error| ExecutionError::Build(error.to_string()))?,
    );
    log_run_started(
        &logger,
        &goals,
        jobs,
        reachable,
        reachable_builders,
        reachable_sources,
        progress,
    );
    let realized = dynamic.realize_goals().await;
    let shutdown = executor
        .shutdown()
        .await
        .map_err(|error| ExecutionError::Build(error.to_string()));
    let realized = match (realized, shutdown) {
        (Ok(realized), Ok(())) => realized,
        (Ok(_), Err(error)) => {
            log_run_finished(&logger, Err(&error));
            logger.flush();
            return Err(error);
        }
        (Err(error), _) => {
            let error = if error.is_cancelled() {
                ExecutionError::Cancelled(error.to_string())
            } else {
                ExecutionError::Build(error.to_string())
            };
            log_run_finished(&logger, Err(&error));
            logger.flush();
            return Err(error);
        }
    };
    let results = goals
        .into_iter()
        .zip(realized)
        .map(|(node, (_key, object_hash))| GoalResult { node, object_hash })
        .collect::<Vec<_>>();
    log_run_finished(&logger, Ok(&results));
    logger.flush();
    Ok(results)
}

struct ResizeMonitor(tokio::task::JoinHandle<()>);

impl ResizeMonitor {
    fn spawn(logger: &Arc<BuildRunLogger>) -> Self {
        let logger = Arc::downgrade(logger);
        Self(tokio::spawn(async move {
            let Ok(mut signal) = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::from_raw(libc::SIGWINCH),
            ) else {
                return;
            };
            while signal.recv().await.is_some() {
                let Some(logger) = logger.upgrade() else {
                    return;
                };
                logger.refresh_progress_layout();
            }
        }))
    }
}

impl Drop for ResizeMonitor {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn check_same_filesystem(store: &Store, run: &Run) -> Result<(), ExecutionError> {
    let store_dev = fs::metadata(store.root())
        .map_err(|error| ExecutionError::Store(error.to_string()))?
        .dev();
    let work_dev = fs::metadata(run.work_dir())
        .map_err(|error| ExecutionError::Run(error.to_string()))?
        .dev();
    if store_dev != work_dev {
        return Err(ExecutionError::InvalidRequest(format!(
            "work directory '{}' must be on the same filesystem as store '{}'",
            run.work_dir().display(),
            store.root().display()
        )));
    }
    Ok(())
}

fn check_content_source_filesystem(
    working: &Store,
    source: &ReadOnlyStore,
    name: &str,
) -> Result<(), ExecutionError> {
    if working.root() == source.root() {
        return Err(ExecutionError::InvalidRequest(format!(
            "secondary content source '{name}' is the working store itself"
        )));
    }
    let working_dev = fs::metadata(working.root())
        .map_err(|error| ExecutionError::Store(error.to_string()))?
        .dev();
    let source_dev = fs::metadata(source.root())
        .map_err(|error| ExecutionError::Store(error.to_string()))?
        .dev();
    if working_dev != source_dev {
        return Err(ExecutionError::InvalidRequest(format!(
            "secondary content source '{name}' store '{}' is on a different filesystem from working store '{}'; hardlink-only content sources require one filesystem",
            source.root().display(),
            working.root().display()
        )));
    }
    Ok(())
}

fn map_graph_error(error: GraphPlanError) -> ExecutionError {
    match error.kind() {
        GraphPlanErrorKind::RequestLoad => ExecutionError::RequestLoad(error.to_string()),
        GraphPlanErrorKind::UnknownBuilder => ExecutionError::UnknownBuilder(error.to_string()),
        GraphPlanErrorKind::InvalidRequest => ExecutionError::InvalidRequest(error.to_string()),
    }
}

fn map_store_error(error: bobr_store::StoreError) -> ExecutionError {
    ExecutionError::Store(error.to_string())
}

fn log_run_started(
    logger: &BuildRunLogger,
    goals: &[String],
    jobs: usize,
    reachable: usize,
    reachable_builders: usize,
    reachable_sources: usize,
    progress: bobr_core::ProgressPolicy,
) {
    logger.log_run_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        status: BuildStatus::RunStarted,
        op: Some("realize".to_string()),
        message: format!("realizing {} goal(s)", goals.len()),
        object_hash: None,
        raw_log_path: None,
        details: json!({
            "goals": goals,
            "jobs": jobs,
            "reachable": reachable,
            "reachable_builders": reachable_builders,
            "reachable_sources": reachable_sources,
            "progress_policy": progress,
        })
        .as_object()
        .expect("run-start details are an object")
        .clone(),
    });
}

fn log_run_finished(logger: &BuildRunLogger, result: Result<&[GoalResult], &ExecutionError>) {
    let stats = logger.outcome_stats();
    let (level, status, message, mut details) = match result {
        Ok(goals) => (
            BuildLogLevel::Info,
            BuildStatus::RunFinished,
            format!("realized {} goal(s)", goals.len()),
            json!({ "goals": goals }),
        ),
        Err(error) => (
            BuildLogLevel::Error,
            BuildStatus::RunFinished,
            error.to_string(),
            json!({ "error_class": error.class() }),
        ),
    };
    details["built"] = json!(stats.built);
    details["cache_hit"] = json!(stats.cache_hit);
    details["failed"] = json!(stats.failed);
    details["cancelled"] = json!(stats.cancelled);
    details["downloaded"] = json!(stats.downloaded);
    details["local"] = json!(stats.local);
    details["secondary"] = json!(stats.secondary);
    details["already_present"] = json!(stats.already_present);
    details["logging_errors"] = json!(logger.logging_errors());
    let retries = logger.download_retries();
    if !retries.is_empty() {
        details["download_retries"] = json!(retries.values().sum::<u64>());
        details["download_retries_by_host"] = json!(retries);
        details["download_retry_reasons"] = json!(logger.download_retry_reasons());
    }
    logger.log_run_event(BuildLogEvent {
        level,
        status,
        op: Some("realize".to_string()),
        message,
        object_hash: None,
        raw_log_path: None,
        details: details
            .as_object()
            .expect("run-finish details are an object")
            .clone(),
    });
}
