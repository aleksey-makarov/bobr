//! Shared network scheduling for source origins and remote repositories.

use crate::http::{HttpOriginError, HttpRetryPolicy, Retry};
use async_trait::async_trait;
use bobr_core::CancellationToken;
use bobr_repo::{
    FetchProgressObserver, FetchPurpose, FetchRequest, FetchResult, RepositoryError,
    RepositoryTransport,
};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// Fully resolved run-wide and per-host network concurrency limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkLimits {
    pub(crate) per_host_default: u32,
    pub(crate) per_host: BTreeMap<String, u32>,
    pub(crate) max_connections: u32,
}

impl NetworkLimits {
    pub(crate) fn for_host(&self, host: &str) -> u32 {
        self.per_host
            .get(host)
            .copied()
            .unwrap_or(self.per_host_default)
            .max(1)
    }
}

/// Logical identity of one scheduled network operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkOperation {
    /// Download of one recipe Source from one of its configured URLs.
    Source {
        /// Recipe-visible source name.
        name: String,
    },
    /// Mutable or immutable remote-repository metadata.
    RepositoryMetadata {
        /// Configured repository name.
        repository: String,
        /// Exact repository value being fetched.
        purpose: FetchPurpose,
    },
    /// Encoded object or filesystem-file repository content.
    RepositoryContent {
        /// Configured repository name.
        repository: String,
        /// Exact repository value being fetched.
        purpose: FetchPurpose,
    },
}

impl NetworkOperation {
    fn repository(repository: String, purpose: FetchPurpose) -> Self {
        match purpose {
            FetchPurpose::Master
            | FetchPurpose::BuildIndex(_)
            | FetchPurpose::ReuseIndex(_)
            | FetchPurpose::ObjectList(_)
            | FetchPurpose::FsFileList(_) => Self::RepositoryMetadata {
                repository,
                purpose,
            },
            FetchPurpose::Object(_) | FetchPurpose::FsFile(_) => Self::RepositoryContent {
                repository,
                purpose,
            },
        }
    }
}

/// One lifecycle update from the common network scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEvent {
    /// Logical operation producing the event.
    pub operation: NetworkOperation,
    /// Host against whose concurrency limit the operation is charged.
    pub host: String,
    /// Complete URL used for this attempt.
    pub url: String,
    /// Lifecycle state and measurements.
    pub kind: NetworkEventKind,
}

/// Lifecycle state of a scheduled network operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkEventKind {
    /// A permit was acquired and one network attempt is starting.
    Started {
        /// One-based attempt number for this URL.
        attempt: u32,
    },
    /// A response body is being transferred.
    Progress {
        /// One-based attempt number for this URL.
        attempt: u32,
        /// Cumulative bytes received during this attempt.
        bytes: u64,
        /// Advertised response length, when known.
        total_bytes: Option<u64>,
    },
    /// A transient failure will be retried after releasing its permits.
    Retry {
        /// One-based number of the next attempt.
        attempt: u32,
        /// Total retry budget for this URL.
        attempts: u32,
        /// Backoff duration before the next attempt.
        delay: Duration,
        /// Human-readable reason for retrying.
        error: String,
    },
    /// One attempt completed successfully.
    Finished {
        /// One-based attempt number for this URL.
        attempt: u32,
        /// Total response bytes received.
        bytes: u64,
        /// Advertised response length, when known.
        total_bytes: Option<u64>,
        /// Wall-clock duration of the successful attempt.
        duration: Duration,
    },
    /// The final permitted attempt failed.
    Failed {
        /// One-based attempt number for this URL.
        attempt: u32,
        /// Human-readable failure.
        error: String,
    },
    /// Cancellation stopped permit waiting, backoff, or active transfer.
    Cancelled,
}

/// Receives typed scheduler events without coupling scheduling to a UI.
pub trait NetworkEventSink: fmt::Debug + Send + Sync {
    /// Records one lifecycle update.
    fn event(&self, event: NetworkEvent);
}

#[derive(Clone)]
pub(crate) struct ScheduledAttempts {
    pub(crate) operation: NetworkOperation,
    pub(crate) url: String,
    pub(crate) policy: HttpRetryPolicy,
    pub(crate) attempts_here: u32,
    pub(crate) spent_before: u32,
    pub(crate) events: Arc<dyn NetworkEventSink>,
}

/// Shared per-run network scheduler.
///
/// Source origins and repository transports use the same run-wide and per-host
/// semaphores. HTTP clients remain transport-owned so repositories may carry
/// independent TLS and CA configuration.
#[derive(Debug, Clone)]
pub struct NetworkScheduler {
    inner: Arc<NetworkSchedulerInner>,
}

#[derive(Debug)]
struct NetworkSchedulerInner {
    limits: NetworkLimits,
    global: Arc<Semaphore>,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
    cancellation: CancellationToken,
    cancel_rx: watch::Receiver<bool>,
    cancel_tx: watch::Sender<bool>,
}

impl NetworkScheduler {
    /// Creates a scheduler for one realization run.
    pub fn new(limits: NetworkLimits, cancellation: CancellationToken) -> Self {
        let (cancel_tx, cancel_rx) = watch::channel(cancellation.is_cancelled());
        Self {
            inner: Arc::new(NetworkSchedulerInner {
                global: Arc::new(Semaphore::new(limits.max_connections.max(1) as usize)),
                limits,
                hosts: Mutex::new(HashMap::new()),
                cancellation,
                cancel_rx,
                cancel_tx,
            }),
        }
    }

    /// Signals cancellation to permit waiters, backoff waits, and transfers.
    pub fn cancel(&self) {
        self.inner.cancellation.cancel();
        let _ = self.inner.cancel_tx.send(true);
    }

    /// Returns whether this run has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancellation.is_cancelled()
    }

    pub(crate) async fn until_cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut receiver = self.inner.cancel_rx.clone();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Acquires one per-host and one run-wide permit.
    pub(crate) async fn acquire(
        &self,
        host: &str,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), NetworkCancelled> {
        if self.is_cancelled() {
            return Err(NetworkCancelled);
        }
        let host_semaphore = {
            let mut hosts = self
                .inner
                .hosts
                .lock()
                .expect("host semaphore map poisoned");
            hosts
                .entry(host.to_string())
                .or_insert_with(|| {
                    Arc::new(Semaphore::new(self.inner.limits.for_host(host) as usize))
                })
                .clone()
        };
        let host_permit = tokio::select! {
            _ = self.until_cancelled() => return Err(NetworkCancelled),
            permit = host_semaphore.acquire_owned() => {
                permit.expect("host semaphore is never closed")
            }
        };
        let global_permit = tokio::select! {
            _ = self.until_cancelled() => return Err(NetworkCancelled),
            permit = self.inner.global.clone().acquire_owned() => {
                permit.expect("global semaphore is never closed")
            }
        };
        if self.is_cancelled() {
            return Err(NetworkCancelled);
        }
        Ok((host_permit, global_permit))
    }

    pub(crate) async fn run_attempts<T, E, F, Fut>(
        &self,
        scheduled: ScheduledAttempts,
        mut attempt: F,
    ) -> Result<T, (E, u32)>
    where
        E: ScheduledNetworkError,
        F: FnMut(NetworkTransferProgress) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let host = crate::http::url_host(&scheduled.url).to_string();
        for attempt_index in 1..=scheduled.attempts_here {
            let attempt_number = scheduled.spent_before + attempt_index;
            let permits = match self.acquire(&host).await {
                Ok(permits) => permits,
                Err(_) => {
                    emit(
                        scheduled.events.as_ref(),
                        &scheduled.operation,
                        &host,
                        &scheduled.url,
                        NetworkEventKind::Cancelled,
                    );
                    return Err((E::cancelled(), attempt_index.saturating_sub(1)));
                }
            };
            emit(
                scheduled.events.as_ref(),
                &scheduled.operation,
                &host,
                &scheduled.url,
                NetworkEventKind::Started {
                    attempt: attempt_number,
                },
            );
            let started = Instant::now();
            let progress = NetworkTransferProgress::new(
                scheduled.operation.clone(),
                host.clone(),
                scheduled.url.clone(),
                attempt_number,
                scheduled.events.clone(),
            );
            let result = tokio::select! {
                _ = self.until_cancelled() => Err(E::cancelled()),
                result = attempt(progress.clone()) => result,
            };
            drop(permits);
            match result {
                Ok(value) => {
                    let (bytes, total_bytes) = progress.snapshot();
                    emit(
                        scheduled.events.as_ref(),
                        &scheduled.operation,
                        &host,
                        &scheduled.url,
                        NetworkEventKind::Finished {
                            attempt: attempt_number,
                            bytes,
                            total_bytes,
                            duration: started.elapsed(),
                        },
                    );
                    return Ok(value);
                }
                Err(error) if error.is_cancelled() || self.is_cancelled() => {
                    emit(
                        scheduled.events.as_ref(),
                        &scheduled.operation,
                        &host,
                        &scheduled.url,
                        NetworkEventKind::Cancelled,
                    );
                    return Err((E::cancelled(), attempt_index));
                }
                Err(error) => {
                    let Retry::After(retry_after) = error.retry() else {
                        emit_failed(
                            scheduled.events.as_ref(),
                            &scheduled.operation,
                            &host,
                            &scheduled.url,
                            attempt_number,
                            &error,
                        );
                        return Err((error, attempt_index));
                    };
                    if attempt_index == scheduled.attempts_here {
                        emit_failed(
                            scheduled.events.as_ref(),
                            &scheduled.operation,
                            &host,
                            &scheduled.url,
                            attempt_number,
                            &error,
                        );
                        return Err((error, attempt_index));
                    }
                    let delay = scheduled.policy.delay_before(
                        attempt_number + 1,
                        retry_after,
                        &scheduled.url,
                    );
                    if self
                        .wait_before_retry(
                            &scheduled.operation,
                            &host,
                            &scheduled.url,
                            attempt_number + 1,
                            scheduled.policy.attempts,
                            delay,
                            &error.to_string(),
                            scheduled.events.as_ref(),
                        )
                        .await
                        .is_err()
                    {
                        return Err((E::cancelled(), attempt_index));
                    }
                }
            }
        }
        unreachable!("at least one network attempt is required")
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn wait_before_retry(
        &self,
        operation: &NetworkOperation,
        host: &str,
        url: &str,
        attempt: u32,
        attempts: u32,
        delay: Duration,
        error: &str,
        events: &dyn NetworkEventSink,
    ) -> Result<(), NetworkCancelled> {
        emit(
            events,
            operation,
            host,
            url,
            NetworkEventKind::Retry {
                attempt,
                attempts,
                delay,
                error: error.to_string(),
            },
        );
        tokio::select! {
            _ = self.until_cancelled() => Err(NetworkCancelled),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }
}

/// Cancellation while waiting for or using a network slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NetworkCancelled;

pub(crate) trait ScheduledNetworkError: fmt::Display + Sized {
    fn retry(&self) -> Retry;
    fn cancelled() -> Self;
    fn is_cancelled(&self) -> bool;
}

impl ScheduledNetworkError for HttpOriginError {
    fn retry(&self) -> Retry {
        self.retry()
    }

    fn cancelled() -> Self {
        HttpOriginError::cancelled("download cancelled")
    }

    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

impl ScheduledNetworkError for RepositoryError {
    fn retry(&self) -> Retry {
        if self.is_retryable() {
            Retry::After(self.retry_after())
        } else {
            Retry::Never
        }
    }

    fn cancelled() -> Self {
        RepositoryError::cancelled("repository transfer cancelled")
    }

    fn is_cancelled(&self) -> bool {
        matches!(self.kind(), bobr_repo::RepositoryErrorKind::Cancelled)
    }
}

#[derive(Debug)]
struct TransferState {
    bytes: u64,
    total_bytes: Option<u64>,
    last_event: Option<Instant>,
}

/// Per-attempt streaming progress handle supplied to a transport.
#[derive(Debug, Clone)]
pub(crate) struct NetworkTransferProgress {
    operation: NetworkOperation,
    host: String,
    url: String,
    attempt: u32,
    events: Arc<dyn NetworkEventSink>,
    state: Arc<Mutex<TransferState>>,
}

impl NetworkTransferProgress {
    fn new(
        operation: NetworkOperation,
        host: String,
        url: String,
        attempt: u32,
        events: Arc<dyn NetworkEventSink>,
    ) -> Self {
        Self {
            operation,
            host,
            url,
            attempt,
            events,
            state: Arc::new(Mutex::new(TransferState {
                bytes: 0,
                total_bytes: None,
                last_event: None,
            })),
        }
    }

    pub(crate) fn transferred(&self, bytes: u64, total_bytes: Option<u64>) {
        let should_emit = {
            let mut state = self.state.lock().expect("network progress state poisoned");
            state.bytes = bytes;
            state.total_bytes = total_bytes;
            if state
                .last_event
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
            {
                state.last_event = Some(Instant::now());
                true
            } else {
                false
            }
        };
        if should_emit {
            emit(
                self.events.as_ref(),
                &self.operation,
                &self.host,
                &self.url,
                NetworkEventKind::Progress {
                    attempt: self.attempt,
                    bytes,
                    total_bytes,
                },
            );
        }
    }

    fn snapshot(&self) -> (u64, Option<u64>) {
        let state = self.state.lock().expect("network progress state poisoned");
        (state.bytes, state.total_bytes)
    }
}

#[derive(Debug)]
struct RepositoryProgressObserver {
    scheduled: NetworkTransferProgress,
    caller: Option<Arc<dyn FetchProgressObserver>>,
}

impl FetchProgressObserver for RepositoryProgressObserver {
    fn transferred(&self, bytes: u64, total_bytes: Option<u64>) {
        self.scheduled.transferred(bytes, total_bytes);
        if let Some(caller) = &self.caller {
            caller.transferred(bytes, total_bytes);
        }
    }
}

/// Repository transport decorator using the realization's network scheduler.
#[derive(Clone)]
pub struct ScheduledRepositoryTransport {
    repository: String,
    inner: Arc<dyn RepositoryTransport>,
    scheduler: NetworkScheduler,
    events: Arc<dyn NetworkEventSink>,
    policy: HttpRetryPolicy,
}

impl fmt::Debug for ScheduledRepositoryTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledRepositoryTransport")
            .field("repository", &self.repository)
            .field("scheduler", &self.scheduler)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl ScheduledRepositoryTransport {
    /// Wraps one transport with shared limits, retry, cancellation, and events.
    pub fn new(
        repository: impl Into<String>,
        inner: Arc<dyn RepositoryTransport>,
        scheduler: NetworkScheduler,
        events: Arc<dyn NetworkEventSink>,
    ) -> Self {
        Self {
            repository: repository.into(),
            inner,
            scheduler,
            events,
            policy: HttpRetryPolicy::production(),
        }
    }

    #[cfg(test)]
    fn with_policy(mut self, policy: HttpRetryPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[async_trait]
impl RepositoryTransport for ScheduledRepositoryTransport {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
        let operation = NetworkOperation::repository(self.repository.clone(), request.purpose);
        let url = request.url.to_string();
        let destination = request.destination.clone();
        let result = self
            .scheduler
            .run_attempts(
                ScheduledAttempts {
                    operation,
                    url,
                    policy: self.policy,
                    attempts_here: self.policy.attempts,
                    spent_before: 0,
                    events: self.events.clone(),
                },
                |progress| {
                    let inner = self.inner.clone();
                    let mut request = request.clone();
                    let caller = request.progress.take();
                    request.progress = Some(Arc::new(RepositoryProgressObserver {
                        scheduled: progress,
                        caller,
                    }));
                    async move { inner.fetch(request).await }
                },
            )
            .await;
        match result {
            Ok(result) => Ok(result),
            Err((error, _)) => {
                remove_partial(&destination).await?;
                Err(error)
            }
        }
    }
}

async fn remove_partial(path: &Path) -> Result<(), RepositoryError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RepositoryError::local_io(format!(
            "failed to remove partial repository response '{}': {error}",
            path.display()
        ))),
    }
}

fn emit(
    events: &dyn NetworkEventSink,
    operation: &NetworkOperation,
    host: &str,
    url: &str,
    kind: NetworkEventKind,
) {
    events.event(NetworkEvent {
        operation: operation.clone(),
        host: host.to_string(),
        url: url.to_string(),
        kind,
    });
}

fn emit_failed(
    events: &dyn NetworkEventSink,
    operation: &NetworkOperation,
    host: &str,
    url: &str,
    attempt: u32,
    error: &dyn fmt::Display,
) {
    emit(
        events,
        operation,
        host,
        url,
        NetworkEventKind::Failed {
            attempt,
            error: error.to_string(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::ObjectHash;
    use bobr_repo::RepresentationMetadata;
    use reqwest::Url;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::time::{sleep, timeout};

    #[derive(Debug, Default)]
    struct RecordedEvents(Mutex<Vec<NetworkEvent>>);

    impl NetworkEventSink for RecordedEvents {
        fn event(&self, event: NetworkEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn scheduler(
        max_connections: u32,
        per_host_default: u32,
        cancellation: CancellationToken,
    ) -> NetworkScheduler {
        NetworkScheduler::new(
            NetworkLimits {
                per_host_default,
                per_host: BTreeMap::new(),
                max_connections,
            },
            cancellation,
        )
    }

    #[tokio::test]
    async fn global_and_per_host_limits_are_shared() {
        let scheduler = scheduler(2, 1, CancellationToken::new());
        let same_host = scheduler.acquire("one.example").await.unwrap();
        let other_host = scheduler.acquire("two.example").await.unwrap();

        assert!(
            timeout(
                Duration::from_millis(20),
                scheduler.acquire("three.example")
            )
            .await
            .is_err(),
            "the global limit must apply across hosts"
        );

        drop(other_host);
        let third_host = scheduler.acquire("three.example").await.unwrap();
        drop(third_host);
        assert!(
            timeout(Duration::from_millis(20), scheduler.acquire("one.example"))
                .await
                .is_err(),
            "the per-host limit must apply within one host"
        );
        drop(same_host);
        assert!(scheduler.acquire("one.example").await.is_ok());
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_permit_wait() {
        let cancellation = CancellationToken::new();
        let scheduler = scheduler(1, 1, cancellation);
        let _permit = scheduler.acquire("one.example").await.unwrap();
        let waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire("two.example").await })
        };
        tokio::task::yield_now().await;
        scheduler.cancel();
        assert!(
            timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[derive(Debug)]
    struct FlakyTransport {
        calls: AtomicUsize,
        retryable: bool,
    }

    #[async_trait]
    impl RepositoryTransport for FlakyTransport {
        async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let body: &[u8] = if call == 0 { b"bad" } else { b"good" };
            tokio::fs::write(&request.destination, body).await?;
            if let Some(progress) = &request.progress {
                progress.transferred(if call == 0 { 3 } else { 4 }, Some(4));
            }
            if call == 0 {
                return Err(RepositoryError::transport(
                    "temporary repository failure",
                    self.retryable,
                ));
            }
            Ok(FetchResult::Stored(RepresentationMetadata::default()))
        }
    }

    fn object_request(destination: &Path) -> FetchRequest {
        FetchRequest {
            purpose: FetchPurpose::Object(
                ObjectHash::from_str(&"1".repeat(64)).expect("valid object hash"),
            ),
            url: Url::parse("https://repo.example/o/object").unwrap(),
            destination: destination.to_path_buf(),
            max_bytes: 1024,
            if_none_match: None,
            progress: None,
        }
    }

    #[tokio::test]
    async fn repository_transport_retries_and_emits_content_progress() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("response");
        let inner = Arc::new(FlakyTransport {
            calls: AtomicUsize::new(0),
            retryable: true,
        });
        let events = Arc::new(RecordedEvents::default());
        let transport = ScheduledRepositoryTransport::new(
            "primary",
            inner.clone(),
            scheduler(1, 1, CancellationToken::new()),
            events.clone(),
        )
        .with_policy(HttpRetryPolicy::instant());

        transport.fetch(object_request(&destination)).await.unwrap();

        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"good");
        let events = events.0.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, NetworkEventKind::Started { .. }))
                .count(),
            2
        );
        assert!(events.iter().any(|event| matches!(
            event.kind,
            NetworkEventKind::Retry {
                attempt: 2,
                attempts: 4,
                ..
            }
        )));
        assert!(events.iter().any(|event| {
            matches!(
                (&event.operation, &event.kind),
                (
                    NetworkOperation::RepositoryContent { repository, .. },
                    NetworkEventKind::Progress {
                        bytes: 4,
                        total_bytes: Some(4),
                        ..
                    }
                ) if repository == "primary"
            )
        }));
    }

    #[tokio::test]
    async fn non_retryable_repository_failure_removes_partial_output() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("response");
        let inner = Arc::new(FlakyTransport {
            calls: AtomicUsize::new(0),
            retryable: false,
        });
        let events = Arc::new(RecordedEvents::default());
        let transport = ScheduledRepositoryTransport::new(
            "primary",
            inner.clone(),
            scheduler(1, 1, CancellationToken::new()),
            events,
        )
        .with_policy(HttpRetryPolicy::instant());

        let error = transport
            .fetch(object_request(&destination))
            .await
            .unwrap_err();

        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert!(!error.is_retryable());
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn cancellation_drops_an_active_repository_transfer_and_removes_partial_output() {
        #[derive(Debug)]
        struct HangingTransport;

        #[async_trait]
        impl RepositoryTransport for HangingTransport {
            async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
                tokio::fs::write(&request.destination, b"partial").await?;
                std::future::pending().await
            }
        }

        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("response");
        let scheduler = scheduler(1, 1, CancellationToken::new());
        let events = Arc::new(RecordedEvents::default());
        let transport = ScheduledRepositoryTransport::new(
            "primary",
            Arc::new(HangingTransport),
            scheduler.clone(),
            events.clone(),
        );
        let task_destination = destination.clone();
        let task =
            tokio::spawn(async move { transport.fetch(object_request(&task_destination)).await });
        for _ in 0..100 {
            if destination.exists() {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert!(destination.exists(), "repository transfer did not start");

        scheduler.cancel();
        let error = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();

        assert_eq!(error.kind(), bobr_repo::RepositoryErrorKind::Cancelled);
        assert!(!destination.exists());
        assert!(
            events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event.kind, NetworkEventKind::Cancelled))
        );
    }

    #[tokio::test]
    async fn retry_backoff_does_not_hold_a_network_slot() {
        #[derive(Debug)]
        struct RetryOnce(AtomicUsize);

        #[async_trait]
        impl RepositoryTransport for RetryOnce {
            async fn fetch(&self, _request: FetchRequest) -> Result<FetchResult, RepositoryError> {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(RepositoryError::retryable_transport(
                        "retry later",
                        Some(Duration::from_millis(50)),
                    ))
                } else {
                    Ok(FetchResult::Missing)
                }
            }
        }

        let temp = TempDir::new().unwrap();
        let scheduler = scheduler(1, 1, CancellationToken::new());
        let events = Arc::new(RecordedEvents::default());
        let transport = ScheduledRepositoryTransport::new(
            "primary",
            Arc::new(RetryOnce(AtomicUsize::new(0))),
            scheduler.clone(),
            events.clone(),
        )
        .with_policy(HttpRetryPolicy::exact(2, Duration::from_millis(200)));
        let destination = temp.path().join("response");
        let task = tokio::spawn(async move { transport.fetch(object_request(&destination)).await });

        let mut retry_started = false;
        for _ in 0..100 {
            if events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event.kind, NetworkEventKind::Retry { .. }))
            {
                retry_started = true;
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert!(retry_started, "repository attempt never entered backoff");
        let permit = timeout(
            Duration::from_millis(50),
            scheduler.acquire("other.example"),
        )
        .await
        .expect("backoff must not hold the global permit")
        .unwrap();
        drop(permit);
        assert!(task.await.unwrap().is_ok());
    }

    #[test]
    fn repository_purposes_are_split_into_metadata_and_content() {
        assert!(matches!(
            NetworkOperation::repository("r".to_string(), FetchPurpose::Master),
            NetworkOperation::RepositoryMetadata { .. }
        ));
        assert!(matches!(
            NetworkOperation::repository(
                "r".to_string(),
                FetchPurpose::Object(ObjectHash::from_str(&"2".repeat(64)).unwrap())
            ),
            NetworkOperation::RepositoryContent { .. }
        ));
    }
}
