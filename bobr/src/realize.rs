use crate::error::ExecutionError;
use crate::request::{
    LocalTransferPolicy, ProviderBackend, ProviderCapability, ProviderConfig, Request,
};
use bobr_core::{
    BuildLogEvent, BuildLogLevel, BuildRunLogger, BuildStatus, CancellationToken, ObjectHash, Run,
    SubjectIdentity,
};
use bobr_repo::{
    FetchPurpose, HttpTransport, ReaderPolicy, RepositoryReader, RepositoryTlsConfig, TrustedKeys,
};
use bobr_runtime::runtime_provider::{RuntimeProvider, runtime_provider_for_current_process};
use bobr_source::build_executor::BuildExecutor;
use bobr_source::dynamic_realizer::DynamicRealizer;
use bobr_source::graph::{GraphPlanError, GraphPlanErrorKind, plan_graph};
use bobr_source::{
    LocalBackendRegistry, LocalContentProvider, LocalIoScheduler, LocalMappingProvider,
    NamedContentProvider, NamedMappingProvider, NetworkEvent, NetworkEventKind, NetworkEventSink,
    NetworkOperation, NetworkScheduler, RemoteBackendRegistry, RemoteContentProvider,
    RemoteMappingProvider, RemoteRepositoryBackend, ScheduledRepositoryTransport,
    SecondaryResolver,
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
    let local_io = LocalIoScheduler::new(limits.resolved_max_local_jobs(), cancellation.clone())
        .map_err(map_store_error)?;
    let network = NetworkScheduler::new(limits.resolved_network_limits(), cancellation.clone());
    let providers = open_providers(
        &store,
        secondaries.providers,
        &secondaries.repository_cache,
        network.clone(),
        limits.resolved_max_local_jobs(),
        logger.clone(),
    )?;
    let provider_log = provider_log_details(&providers);
    let (mapping_providers, content_providers) = provider_capabilities(
        providers,
        store.clone(),
        runtime_provider.clone(),
        local_io.clone(),
    );
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

struct OpenedProvider {
    name: String,
    capability: ProviderCapability,
    backend: OpenedProviderBackend,
}

enum OpenedProviderBackend {
    Local {
        transfer: Option<LocalTransferPolicy>,
        repository: LocalRepository,
    },
    Remote {
        master_url: url::Url,
        repository: RemoteRepositoryBackend,
    },
}

struct RepositoryNetworkEvents {
    logger: Arc<BuildRunLogger>,
}

impl std::fmt::Debug for RepositoryNetworkEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryNetworkEvents")
            .finish_non_exhaustive()
    }
}

impl NetworkEventSink for RepositoryNetworkEvents {
    fn event(&self, event: NetworkEvent) {
        let (repository, purpose, class) = match &event.operation {
            NetworkOperation::RepositoryMetadata {
                repository,
                purpose,
            } => (repository, purpose, "metadata"),
            NetworkOperation::RepositoryContent {
                repository,
                purpose,
            } => (repository, purpose, "content"),
            NetworkOperation::Source { .. } => return,
        };
        let (identity, purpose_name, object_hash) = repository_subject(repository, *purpose, class);
        let mut details = json!({
            "network_lifecycle": true,
            "transfer": "network",
            "repository": repository,
            "repository_class": class,
            "repository_purpose": purpose_name,
            "host": event.host,
        })
        .as_object()
        .expect("repository network details are an object")
        .clone();
        let (level, status, message) = match event.kind {
            NetworkEventKind::Started { attempt } => {
                details.insert("attempt".to_string(), json!(attempt));
                details.insert("bytes".to_string(), json!(0));
                (
                    BuildLogLevel::Info,
                    BuildStatus::Running,
                    format!("fetching {purpose_name} from {}", event.host),
                )
            }
            NetworkEventKind::Progress {
                attempt,
                bytes,
                total_bytes,
            } => {
                details.insert("attempt".to_string(), json!(attempt));
                details.insert("bytes".to_string(), json!(bytes));
                details.insert("encoded_bytes".to_string(), json!(bytes));
                if let Some(total_bytes) = total_bytes {
                    details.insert("total_bytes".to_string(), json!(total_bytes));
                }
                (
                    BuildLogLevel::Progress,
                    BuildStatus::Running,
                    format!(
                        "received {bytes} bytes of {purpose_name} from {}",
                        event.host
                    ),
                )
            }
            NetworkEventKind::Retry {
                attempt,
                attempts,
                delay,
                error,
            } => {
                details.insert("attempt".to_string(), json!(attempt));
                details.insert("attempts".to_string(), json!(attempts));
                details.insert("retry_host".to_string(), json!(event.host));
                details.insert("retry_delay_ms".to_string(), json!(duration_ms(delay)));
                details.insert("retry_reason".to_string(), json!(error));
                (
                    BuildLogLevel::Info,
                    BuildStatus::Running,
                    format!(
                        "retrying {purpose_name} from {} in {:.1}s (attempt {attempt} of {attempts})",
                        event.host,
                        delay.as_secs_f64(),
                    ),
                )
            }
            NetworkEventKind::Finished {
                attempt,
                bytes,
                total_bytes,
                duration,
            } => {
                details.insert("attempt".to_string(), json!(attempt));
                details.insert("bytes".to_string(), json!(bytes));
                details.insert("encoded_bytes".to_string(), json!(bytes));
                details.insert("duration_ms".to_string(), json!(duration_ms(duration)));
                if let Some(total_bytes) = total_bytes {
                    details.insert("total_bytes".to_string(), json!(total_bytes));
                }
                (
                    BuildLogLevel::Info,
                    BuildStatus::Done,
                    format!(
                        "fetched {bytes} bytes of {purpose_name} from {} in {} ms",
                        event.host,
                        duration_ms(duration),
                    ),
                )
            }
            NetworkEventKind::Failed { attempt, error } => {
                details.insert("attempt".to_string(), json!(attempt));
                details.insert("network_error".to_string(), json!(error.clone()));
                (
                    BuildLogLevel::Warn,
                    BuildStatus::Failed,
                    format!(
                        "failed to fetch {purpose_name} from {}: {error}",
                        event.host
                    ),
                )
            }
            NetworkEventKind::Cancelled => (
                BuildLogLevel::Info,
                BuildStatus::Cancelled,
                format!("cancelled {purpose_name} transfer from {}", event.host),
            ),
        };
        self.logger.log_subject_event(
            &identity,
            BuildLogEvent {
                level,
                status,
                op: Some(format!("repository-{class}")),
                message,
                object_hash,
                raw_log_path: None,
                details,
            },
        );
    }
}

fn repository_subject(
    repository: &str,
    purpose: FetchPurpose,
    class: &str,
) -> (SubjectIdentity, String, Option<ObjectHash>) {
    let (tag, kind, value, object_hash) = match purpose {
        FetchPurpose::Master => ("RepositoryMetadata", "master", None, None),
        FetchPurpose::BuildIndex(hash) => (
            "RepositoryMetadata",
            "build index",
            Some(hash.to_string()),
            None,
        ),
        FetchPurpose::ReuseIndex(hash) => (
            "RepositoryMetadata",
            "reuse index",
            Some(hash.to_string()),
            None,
        ),
        FetchPurpose::ObjectList(hash) => (
            "RepositoryMetadata",
            "object list",
            Some(hash.to_string()),
            None,
        ),
        FetchPurpose::FsFileList(hash) => (
            "RepositoryMetadata",
            "filesystem-file list",
            Some(hash.to_string()),
            None,
        ),
        FetchPurpose::Object(hash) => (
            "SecondaryContent",
            "object",
            Some(hash.to_string()),
            Some(hash),
        ),
        FetchPurpose::FsFile(hash) => (
            "SecondaryContent",
            "filesystem file",
            Some(hash.to_string()),
            None,
        ),
    };
    let purpose_name = match &value {
        Some(value) => format!("{kind} {}", &value[..12]),
        None => kind.to_string(),
    };
    let key = match value {
        Some(value) if matches!(purpose, FetchPurpose::Object(_)) => value,
        Some(value) => format!("repository:{class}:{repository}:{kind}:{value}"),
        None => format!("repository:{class}:{repository}:{kind}"),
    };
    (
        SubjectIdentity::new(tag, format!("{repository} {purpose_name}"), key),
        purpose_name,
        object_hash,
    )
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn open_providers(
    working: &Store,
    providers: Vec<ProviderConfig>,
    repository_cache: &std::path::Path,
    network: NetworkScheduler,
    max_local_jobs: usize,
    logger: Arc<BuildRunLogger>,
) -> Result<Vec<OpenedProvider>, ExecutionError> {
    let mut canonical_capabilities = HashMap::new();
    let mut canonical_by_name = HashMap::new();
    let mut local_registry = LocalBackendRegistry::default();
    let mut remote_registry = RemoteBackendRegistry::default();
    let mut opened = Vec::with_capacity(providers.len());
    for provider in providers {
        let backend = match provider.backend {
            ProviderBackend::Local { store, transfer } => {
                let backend = local_registry.open(&store).map_err(map_store_error)?;
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
                OpenedProviderBackend::Local {
                    transfer,
                    repository: backend,
                }
            }
            ProviderBackend::Remote {
                master_url,
                trusted_keys,
                ca_bundle,
            } => {
                let identity = master_url.to_string();
                let repository = remote_registry
                    .open_with(identity.clone(), || {
                        let trusted = TrustedKeys::from_files(&trusted_keys)?;
                        let tls = match &ca_bundle {
                            Some(path) => RepositoryTlsConfig::from_ca_bundle(path)?,
                            None => RepositoryTlsConfig::default_roots(),
                        };
                        let transport = Arc::new(ScheduledRepositoryTransport::new(
                            provider.name.clone(),
                            Arc::new(HttpTransport::anonymous(&tls)?),
                            network.clone(),
                            Arc::new(RepositoryNetworkEvents {
                                logger: logger.clone(),
                            }),
                        ));
                        RepositoryReader::with_policy(
                            master_url.clone(),
                            trusted,
                            repository_cache,
                            transport,
                            ReaderPolicy {
                                max_blocking_decoders: max_local_jobs,
                            },
                        )
                    })
                    .map_err(|error| {
                        ExecutionError::InvalidRequest(format!(
                            "failed to open remote provider '{}': {error}",
                            provider.name
                        ))
                    })?;
                OpenedProviderBackend::Remote {
                    master_url,
                    repository,
                }
            }
        };
        opened.push(OpenedProvider {
            name: provider.name,
            capability: provider.capability,
            backend,
        });
    }
    Ok(opened)
}

fn provider_capabilities(
    providers: Vec<OpenedProvider>,
    working: Store,
    runtime_provider: RuntimeProvider,
    local_io: LocalIoScheduler,
) -> (Vec<NamedMappingProvider>, Vec<NamedContentProvider>) {
    let mut mapping_providers = Vec::new();
    let mut content_providers = Vec::new();
    for provider in providers {
        match (provider.capability, provider.backend) {
            (ProviderCapability::Mappings, OpenedProviderBackend::Local { repository, .. }) => {
                mapping_providers.push(NamedMappingProvider::new(
                    provider.name,
                    Arc::new(LocalMappingProvider::new(
                        Arc::new(LocalTrustedKeyIndex::new(repository)),
                        local_io.clone(),
                    )),
                ))
            }
            (
                ProviderCapability::Content,
                OpenedProviderBackend::Local {
                    transfer,
                    repository,
                },
            ) => {
                let source = match transfer
                    .expect("validated local content provider has a transfer mode")
                {
                    LocalTransferPolicy::Hardlink => {
                        Arc::new(LocalHardlinkContentSource::with_runtime(
                            repository,
                            runtime_provider.clone(),
                        )) as Arc<dyn bobr_store::ContentSource>
                    }
                    LocalTransferPolicy::Copy => Arc::new(LocalCopyContentSource::with_runtime(
                        repository,
                        runtime_provider.clone(),
                    )),
                };
                content_providers.push(NamedContentProvider::new(
                    provider.name,
                    Arc::new(LocalContentProvider::new(source, local_io.clone())),
                ));
            }
            (ProviderCapability::Mappings, OpenedProviderBackend::Remote { repository, .. }) => {
                mapping_providers.push(NamedMappingProvider::new(
                    provider.name,
                    Arc::new(RemoteMappingProvider::new(repository)),
                ))
            }
            (ProviderCapability::Content, OpenedProviderBackend::Remote { repository, .. }) => {
                content_providers.push(NamedContentProvider::new(
                    provider.name,
                    Arc::new(RemoteContentProvider::new(
                        repository,
                        working.clone(),
                        runtime_provider.clone(),
                        local_io.clone(),
                    )),
                ))
            }
        }
    }
    (mapping_providers, content_providers)
}

fn provider_log_details(providers: &[OpenedProvider]) -> Vec<serde_json::Value> {
    providers
        .iter()
        .map(|provider| match &provider.backend {
            OpenedProviderBackend::Local {
                transfer,
                repository,
            } => {
                let mut detail = json!({
                    "name": provider.name,
                    "capability": provider.capability.as_str(),
                    "backend": {
                        "kind": "local",
                        "store": repository.store().root(),
                    },
                });
                if let Some(transfer) = transfer {
                    detail["backend"]["transfer"] = json!(transfer.as_str());
                }
                detail
            }
            OpenedProviderBackend::Remote { master_url, .. } => json!({
                "name": provider.name,
                "capability": provider.capability.as_str(),
                "backend": {
                    "kind": "remote",
                    "master_url": master_url,
                },
            }),
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
    details["remote"] = json!(stats.remote);
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
    use bobr_repo::FetchPurpose;
    use bobr_source::NetworkEventKind;
    use serde_json::Value;
    use std::str::FromStr;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn repository_network_events_are_structured_and_do_not_count_as_failures() {
        let temp = tempdir().unwrap();
        let run_log_dir = temp.path().join("logs");
        let logger = Arc::new(BuildRunLogger::new(&run_log_dir, "run", true).unwrap());
        let sink = RepositoryNetworkEvents {
            logger: logger.clone(),
        };
        let metadata = NetworkOperation::RepositoryMetadata {
            repository: "archive".to_string(),
            purpose: FetchPurpose::Master,
        };
        sink.event(NetworkEvent {
            operation: metadata.clone(),
            host: "repo.example".to_string(),
            url: "https://repo.example/master".to_string(),
            kind: NetworkEventKind::Started { attempt: 1 },
        });
        sink.event(NetworkEvent {
            operation: metadata,
            host: "repo.example".to_string(),
            url: "https://repo.example/master".to_string(),
            kind: NetworkEventKind::Failed {
                attempt: 1,
                error: "connection reset".to_string(),
            },
        });
        let object = ObjectHash::from_str(&"3".repeat(64)).unwrap();
        sink.event(NetworkEvent {
            operation: NetworkOperation::RepositoryContent {
                repository: "archive".to_string(),
                purpose: FetchPurpose::Object(object),
            },
            host: "cdn.example".to_string(),
            url: format!("https://cdn.example/o/{object}"),
            kind: NetworkEventKind::Finished {
                attempt: 1,
                bytes: 123,
                total_bytes: Some(123),
                duration: Duration::from_millis(25),
            },
        });
        logger.flush();

        let events = fs::read_to_string(run_log_dir.join("events.jsonl")).unwrap();
        let records = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["subject"]["tag"], "RepositoryMetadata");
        assert_eq!(records[0]["details"]["repository"], "archive");
        assert_eq!(records[0]["details"]["repository_purpose"], "master");
        assert_eq!(records[1]["level"], "warn");
        assert_eq!(records[2]["subject"]["tag"], "SecondaryContent");
        assert_eq!(records[2]["details"]["encoded_bytes"], 123);
        assert_eq!(logger.outcome_stats().failed, 0);
    }

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

        let (indexes, sources) = provider_capabilities(
            vec![OpenedProvider {
                name: "content-only".to_string(),
                capability: ProviderCapability::Content,
                backend: OpenedProviderBackend::Local {
                    transfer: Some(LocalTransferPolicy::Hardlink),
                    repository: repository.clone(),
                },
            }],
            working.clone(),
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

        let (indexes, sources) = provider_capabilities(
            vec![OpenedProvider {
                name: "mappings-only".to_string(),
                capability: ProviderCapability::Mappings,
                backend: OpenedProviderBackend::Local {
                    transfer: None,
                    repository,
                },
            }],
            working.clone(),
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
