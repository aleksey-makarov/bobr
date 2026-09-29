use crate::error::ExecutionError;
use crate::request::{
    LocalTransferPolicy, ProviderBackend, ProviderCapability, ProviderConfig, Request,
};
use bobr_core::{
    BuildLogEvent, BuildLogLevel, BuildRunLogger, BuildStatus, CancellationToken, ObjectHash, Run,
};
use bobr_runtime::runtime_provider::{RuntimeProvider, runtime_provider_for_current_process};
use bobr_source::build_executor::BuildExecutor;
use bobr_source::dynamic_realizer::DynamicRealizer;
use bobr_source::graph::{GraphPlanError, GraphPlanErrorKind, plan_graph};
use bobr_source::{
    LocalBackendRegistry, LocalContentProvider, LocalIoScheduler, LocalMappingProvider,
    NamedContentProvider, NamedMappingProvider, NetworkScheduler, SecondaryResolver,
};
#[cfg(test)]
use bobr_store::ReadOnlyStore;
use bobr_store::{
    LocalCopyContentSource, LocalHardlinkContentSource, LocalRepository, LocalTrustedKeyIndex,
    Store,
};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
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
    reject_remote_providers(&secondaries.providers)?;
    let store = Store::create(&store_path).map_err(map_store_error)?;
    let providers = open_local_providers(&store, secondaries.providers)?;
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
    let local_io = LocalIoScheduler::new(limits.resolved_max_local_jobs(), cancellation.clone())
        .map_err(map_store_error)?;
    let network = NetworkScheduler::new(limits.resolved_network_limits(), cancellation.clone());
    let provider_log = provider_log_details(&providers);
    let (mapping_providers, content_providers) =
        local_provider_capabilities(providers, runtime_provider.clone(), local_io.clone());
    let secondary = Arc::new(
        SecondaryResolver::new(
            store.clone(),
            run.run_id(),
            mapping_providers,
            content_providers,
        )
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
            local_io,
            network,
        )
        .map_err(|error| ExecutionError::Build(error.to_string()))?,
    );
    log_run_started(
        &logger,
        RunStartedDetails {
            goals: &goals,
            jobs,
            reachable,
            reachable_builders,
            reachable_sources,
            progress,
            providers: &provider_log,
        },
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

struct OpenedLocalProvider {
    name: String,
    capability: ProviderCapability,
    transfer: Option<LocalTransferPolicy>,
    repository: LocalRepository,
}

fn reject_remote_providers(providers: &[ProviderConfig]) -> Result<(), ExecutionError> {
    if let Some(provider) = providers
        .iter()
        .find(|provider| matches!(provider.backend, ProviderBackend::Remote { .. }))
    {
        return Err(ExecutionError::InvalidRequest(format!(
            "remote secondary provider '{}' is not implemented by this build",
            provider.name
        )));
    }
    Ok(())
}

fn open_local_providers(
    working: &Store,
    providers: Vec<ProviderConfig>,
) -> Result<Vec<OpenedLocalProvider>, ExecutionError> {
    let mut canonical_capabilities = HashMap::new();
    let mut canonical_by_name = HashMap::new();
    let mut registry = LocalBackendRegistry::default();
    let mut opened = Vec::with_capacity(providers.len());
    for provider in providers {
        let ProviderBackend::Local { store, transfer } = provider.backend else {
            return Err(ExecutionError::InvalidRequest(format!(
                "remote secondary provider '{}' is not implemented by this build",
                provider.name
            )));
        };
        let backend = registry.open(&store).map_err(map_store_error)?;
        if backend.store().root() == working.root() {
            return Err(ExecutionError::InvalidRequest(format!(
                "local provider '{}' is a canonical alias of the working store '{}'",
                provider.name,
                working.root().display()
            )));
        }
        let canonical_root = backend.store().root().to_path_buf();
        if let Some(previous_name) = canonical_capabilities.insert(
            (provider.capability, canonical_root.clone()),
            provider.name.clone(),
        ) {
            return Err(ExecutionError::InvalidRequest(format!(
                "{} providers '{previous_name}' and '{}' resolve to the same store root '{}'",
                provider.capability.as_str(),
                provider.name,
                backend.store().root().display()
            )));
        }
        if let Some(previous_root) =
            canonical_by_name.insert(provider.name.clone(), canonical_root.clone())
            && previous_root != canonical_root
        {
            return Err(ExecutionError::InvalidRequest(format!(
                "complementary providers named '{}' resolve to different store roots",
                provider.name
            )));
        }
        if transfer == Some(LocalTransferPolicy::Hardlink) {
            backend
                .validate_hardlink_compatible_with(working)
                .map_err(|error| {
                    ExecutionError::InvalidRequest(format!(
                        "local provider '{}' cannot use transfer mode 'hardlink': {error}",
                        provider.name
                    ))
                })?;
        }
        opened.push(OpenedLocalProvider {
            name: provider.name,
            capability: provider.capability,
            transfer,
            repository: backend,
        });
    }
    Ok(opened)
}

fn local_provider_capabilities(
    providers: Vec<OpenedLocalProvider>,
    runtime_provider: RuntimeProvider,
    local_io: LocalIoScheduler,
) -> (Vec<NamedMappingProvider>, Vec<NamedContentProvider>) {
    let mut mapping_providers = Vec::new();
    let mut content_providers = Vec::new();
    for provider in providers {
        match provider.capability {
            ProviderCapability::Mappings => mapping_providers.push(NamedMappingProvider::new(
                provider.name,
                Arc::new(LocalMappingProvider::new(
                    Arc::new(LocalTrustedKeyIndex::new(provider.repository)),
                    local_io.clone(),
                )),
            )),
            ProviderCapability::Content => {
                let source = match provider
                    .transfer
                    .expect("validated local content provider has a transfer mode")
                {
                    LocalTransferPolicy::Hardlink => {
                        Arc::new(LocalHardlinkContentSource::with_runtime(
                            provider.repository,
                            runtime_provider.clone(),
                        )) as Arc<dyn bobr_store::ContentSource>
                    }
                    LocalTransferPolicy::Copy => Arc::new(LocalCopyContentSource::with_runtime(
                        provider.repository,
                        runtime_provider.clone(),
                    )),
                };
                content_providers.push(NamedContentProvider::new(
                    provider.name,
                    Arc::new(LocalContentProvider::new(source, local_io.clone())),
                ));
            }
        }
    }
    (mapping_providers, content_providers)
}

fn provider_log_details(providers: &[OpenedLocalProvider]) -> Vec<serde_json::Value> {
    providers
        .iter()
        .map(|provider| {
            let mut detail = json!({
                "name": provider.name,
                "capability": provider.capability.as_str(),
                "backend": {
                    "kind": "local",
                    "store": provider.repository.store().root(),
                },
            });
            if let Some(transfer) = provider.transfer {
                detail["backend"]["transfer"] = json!(transfer.as_str());
            }
            detail
        })
        .collect()
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

struct RunStartedDetails<'a> {
    goals: &'a [String],
    jobs: usize,
    reachable: usize,
    reachable_builders: usize,
    reachable_sources: usize,
    progress: bobr_core::ProgressPolicy,
    providers: &'a [serde_json::Value],
}

fn log_run_started(logger: &BuildRunLogger, details: RunStartedDetails<'_>) {
    logger.log_run_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        status: BuildStatus::RunStarted,
        op: Some("realize".to_string()),
        message: format!("realizing {} goal(s)", details.goals.len()),
        object_hash: None,
        raw_log_path: None,
        details: json!({
            "goals": details.goals,
            "jobs": details.jobs,
            "reachable": details.reachable,
            "reachable_builders": details.reachable_builders,
            "reachable_sources": details.reachable_sources,
            "progress_policy": details.progress,
            "providers": details.providers,
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
    details["hardlinked"] = json!(stats.hardlinked);
    details["copied"] = json!(stats.copied);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::BuildKey;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[tokio::test]
    async fn local_mapping_and_content_capabilities_are_independent() {
        let temp = tempdir().unwrap();
        let repository_root = temp.path().join("repository");
        let working_root = temp.path().join("working");
        fs::create_dir(&repository_root).unwrap();
        fs::create_dir(&working_root).unwrap();
        Store::create(&repository_root).unwrap();
        let working = Store::create(&working_root).unwrap();
        let build_key = BuildKey::from_str(&"1".repeat(64)).unwrap();
        fs::write(
            repository_root.join("builds").join(build_key.to_string()),
            b"not a symlink\n",
        )
        .unwrap();
        let repository = LocalRepository::new(ReadOnlyStore::open(&repository_root).unwrap());
        let local_io = LocalIoScheduler::new(4, CancellationToken::new()).unwrap();

        let (indexes, sources) = local_provider_capabilities(
            vec![OpenedLocalProvider {
                name: "content-only".to_string(),
                capability: ProviderCapability::Content,
                transfer: Some(LocalTransferPolicy::Hardlink),
                repository: repository.clone(),
            }],
            RuntimeProvider::host(),
            local_io.clone(),
        );
        let resolver =
            SecondaryResolver::new(working.clone(), "content-only-run", indexes, sources).unwrap();
        assert!(!resolver.has_mapping_providers());
        assert!(resolver.has_content_sources());
        let report = resolver
            .resolve_builds(&[build_key])
            .await
            .unwrap()
            .remove(0);
        assert!(report.answers.is_empty());
        assert!(report.resolved.is_none());

        let (indexes, sources) = local_provider_capabilities(
            vec![OpenedLocalProvider {
                name: "mappings-only".to_string(),
                capability: ProviderCapability::Mappings,
                transfer: None,
                repository,
            }],
            RuntimeProvider::host(),
            local_io,
        );
        let resolver =
            SecondaryResolver::new(working, "mappings-only-run", indexes, sources).unwrap();
        assert!(resolver.has_mapping_providers());
        assert!(!resolver.has_content_sources());
        let error = resolver.resolve_builds(&[build_key]).await.unwrap_err();
        assert!(error.to_string().contains("is not a symlink"), "{error}");
    }
}
