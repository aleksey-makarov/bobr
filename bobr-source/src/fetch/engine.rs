//! Downloads every source of a fetch request into the store.
//!
//! All sources start at once; what actually runs is governed by two layers of
//! semaphores -- one per host, one per process -- so a saturated mirror stalls
//! only its own queue while everyone else proceeds. The mirror walk, retry
//! classification and backoff are the same ones the synchronous source path
//! uses; only the transport around them is asynchronous.

use crate::fetch::request::{FetchRequest, ResolvedLimits, SourceEntry};
use crate::http::{
    self, HttpOrigin, HttpOriginError, HttpRetryPolicy, HttpTimeouts, Retry, UrlAttemptState,
};
use crate::origin::OriginContext;
use bobr_core::{
    BuildLogEvent, BuildLogLevel, BuildLogSubject, BuildLogger, BuildRunLogger, BuildStatus,
    CancellationToken, ObjectHash, Run, Workspace,
};
use bobr_store::{SourceImportOutcome, Store, import_source_object, record_existing_source_object};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;

/// What one run of the fetcher did, and what is left for the caller to fix.
#[derive(Debug, Default)]
pub struct Summary {
    /// Sources downloaded and imported this run.
    pub downloaded: u64,
    /// Sources already present in the store; no network was touched for them.
    pub cache_hit: u64,
    /// Sources with a `Path` origin, left to the build to materialize.
    pub path_skipped: u64,
    /// Sources whose download produced a different object than the recipe
    /// declares: the placeholder cycle's payload, every real hash in one run.
    pub mismatched: Vec<Mismatch>,
    /// Sources that could not be fetched, with the reason for each.
    pub failed: Vec<(String, String)>,
    /// Whether the run was cancelled before finishing.
    pub cancelled: bool,
}

/// One source whose downloaded content hashes differently than declared.
#[derive(Debug)]
pub struct Mismatch {
    /// The source's name, as the recipes call it.
    pub name: String,
    /// The hash the recipe declares.
    pub declared: String,
    /// The hash the download actually produced -- the one to paste when the
    /// declared value was a placeholder.
    pub actual: String,
}

impl Summary {
    /// Whether the run finished with nothing left for the caller to fix.
    pub fn is_success(&self) -> bool {
        !self.cancelled && self.mismatched.is_empty() && self.failed.is_empty()
    }
}

/// Everything one source task needs; cloned into each task.
struct Engine {
    store: Store,
    run: Arc<Run>,
    logger: Arc<BuildRunLogger>,
    client: reqwest::Client,
    limits: ResolvedLimits,
    global: Arc<Semaphore>,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
    cancellation: CancellationToken,
    cancel_rx: watch::Receiver<bool>,
    /// Held so `cancel_rx.changed()` cannot resolve by sender-drop; cancelling
    /// goes through [`Engine::cancel`].
    cancel_tx: watch::Sender<bool>,
}

impl Engine {
    fn cancel(&self) {
        self.cancellation.cancel();
        let _ = self.cancel_tx.send(true);
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Resolves once the run is cancelled; pends forever otherwise.
    async fn until_cancelled(&self) {
        let mut rx = self.cancel_rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// One permit against the host's limit and one against the process-wide
    /// cap, in that fixed order everywhere so the two layers cannot deadlock.
    /// Waiting counts as neither an attempt nor a timeout, and cancellation
    /// interrupts it.
    async fn acquire_permits(
        &self,
        host: &str,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), HttpOriginError> {
        let host_semaphore = {
            let mut hosts = self.hosts.lock().expect("host semaphore map poisoned");
            hosts
                .entry(host.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(self.limits.for_host(host) as usize)))
                .clone()
        };
        let host_permit = tokio::select! {
            _ = self.until_cancelled() => return Err(cancelled_error()),
            permit = host_semaphore.acquire_owned() => {
                permit.expect("host semaphore closed")
            }
        };
        let global_permit = tokio::select! {
            _ = self.until_cancelled() => return Err(cancelled_error()),
            permit = self.global.clone().acquire_owned() => {
                permit.expect("global semaphore closed")
            }
        };
        Ok((host_permit, global_permit))
    }
}

fn cancelled_error() -> HttpOriginError {
    HttpOriginError::fatal_network("download cancelled")
}

/// Runs a whole fetch request; the returned [`Summary`] is the exit status in
/// structured form. `Err` is reserved for the run itself being unusable (bad
/// directories, unusable store) -- per-source failures land in the summary.
pub async fn run_fetch(request: FetchRequest) -> Result<Summary, String> {
    let store = Store::create(&request.store).map_err(|error| error.to_string())?;
    let run = Arc::new(
        Run::new(request.run_id.clone(), &request.logs, &request.work)
            .map_err(|error| error.to_string())?,
    );
    check_same_filesystem(&store, &run)?;
    let logger = Arc::new(BuildRunLogger::new(run.logs_dir(), run.run_id(), false)?);

    let limits = ResolvedLimits::from_request(&request.limits);
    let client = http_client(HttpTimeouts::production())?;
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let engine = Arc::new(Engine {
        store,
        run,
        logger: logger.clone(),
        client,
        global: Arc::new(Semaphore::new(limits.max_connections as usize)),
        limits,
        hosts: Mutex::new(HashMap::new()),
        cancellation: CancellationToken::new(),
        cancel_rx,
        cancel_tx,
    });

    // One source per declared hash: the request lowers a graph where several
    // nodes may share a source, and downloading it twice would race on the
    // same object for no gain.
    let mut seen = std::collections::HashSet::new();
    let sources: Vec<SourceEntry> = request
        .sources
        .into_iter()
        .filter(|entry| seen.insert(entry.object_hash.clone()))
        .collect();

    log_run_started(&logger, sources.len(), &engine.limits);

    {
        let engine = engine.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                engine.cancel();
            }
        });
    }

    let mut tasks = JoinSet::new();
    for entry in sources {
        let engine = engine.clone();
        tasks.spawn(async move { process_source(engine, entry).await });
    }

    let mut summary = Summary::default();
    while let Some(joined) = tasks.join_next().await {
        let outcome = joined.map_err(|error| format!("source task panicked: {error}"))?;
        match outcome {
            SourceOutcome::Downloaded => summary.downloaded += 1,
            SourceOutcome::CacheHit => summary.cache_hit += 1,
            SourceOutcome::PathSkipped => summary.path_skipped += 1,
            SourceOutcome::Mismatched(mismatch) => summary.mismatched.push(mismatch),
            SourceOutcome::Failed { name, message } => summary.failed.push((name, message)),
        }
    }
    summary.cancelled = engine.is_cancelled();
    summary.mismatched.sort_by(|a, b| a.name.cmp(&b.name));
    summary.failed.sort();

    log_run_finished(&logger, &summary);
    Ok(summary)
}

enum SourceOutcome {
    Downloaded,
    CacheHit,
    PathSkipped,
    Mismatched(Mismatch),
    Failed { name: String, message: String },
}

async fn process_source(engine: Arc<Engine>, entry: SourceEntry) -> SourceOutcome {
    let name = entry.name.clone();
    match process_source_inner(&engine, entry).await {
        Ok(outcome) => outcome,
        Err(message) => {
            // Every failure is named in the run log, whatever stage it died at:
            // some happen before the source has a workspace to log into, and a
            // count with no names would send the reader digging.
            engine.logger.log_run_event(run_event(
                BuildLogLevel::Error,
                BuildStatus::Failed,
                format!("source '{name}' failed: {message}"),
                None,
            ));
            SourceOutcome::Failed { name, message }
        }
    }
}

async fn process_source_inner(
    engine: &Arc<Engine>,
    entry: SourceEntry,
) -> Result<SourceOutcome, String> {
    // Trimmed like the build's own parser: recipe lock files are imported as
    // text and carry a trailing newline, and Path sources declare their hashes
    // straight from those files.
    let declared = ObjectHash::from_str(entry.object_hash.trim()).map_err(|error| {
        format!(
            "invalid object_hash '{}': {error}",
            entry.object_hash.trim()
        )
    })?;

    // Already in the store: record the ref and move on, exactly what the build
    // does on its own cache hit. This is also why a fetcher pointed at a warm
    // store causes no network traffic at all.
    let hit = {
        let engine = engine.clone();
        let name = entry.name.clone();
        run_blocking(move || {
            record_existing_source_object(&engine.store, declared, &name, engine.run.run_id())
                .map_err(|error| error.to_string())
        })
        .await??
    };
    if hit.is_some() {
        engine.logger.log_run_event(run_event(
            BuildLogLevel::Info,
            BuildStatus::CacheHit,
            format!("source '{}' is already in the store", entry.name),
            Some(declared),
        ));
        return Ok(SourceOutcome::CacheHit);
    }

    let Some(origin_value) = entry.origin.clone() else {
        return Err(format!(
            "source '{}' has no origin and object '{}' is not present in the store",
            entry.name, declared
        ));
    };
    let tag = origin_value
        .get("tag")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("source '{}': origin.tag: expected string", entry.name))?
        .to_string();

    if tag == "Path" {
        // Not handled yet (the build materializes these itself); say so once
        // per source rather than failing a request the recipes legitimately
        // produce.
        engine.logger.log_run_event(run_event(
            BuildLogLevel::Info,
            BuildStatus::Done,
            format!(
                "source '{}' has a Path origin; left to the build",
                entry.name
            ),
            None,
        ));
        return Ok(SourceOutcome::PathSkipped);
    }

    if engine.is_cancelled() {
        return Err("cancelled before starting".to_string());
    }

    let workspace = engine
        .run
        .create_workspace("Source", &entry.name, declared.to_string())
        .map_err(|error| error.to_string())?;
    let subject_logger = engine
        .logger
        .bind_subject(BuildLogSubject::new(
            "Source",
            entry.name.clone(),
            declared.to_string(),
            workspace.log_dir().to_path_buf(),
            workspace.raw_log_dir().to_path_buf(),
        ))
        .map_err(|error| error.to_string())?;
    // The intended host travels with `start`: until the first request goes out
    // the download is queued, and the live log counts what is waiting per host.
    subject_logger.log_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        status: BuildStatus::Start,
        op: None,
        message: "starting subject".to_string(),
        object_hash: None,
        raw_log_path: None,
        details: host_details(&intended_host(&origin_value)),
    });
    log_subject(
        &subject_logger,
        BuildStatus::CacheMiss,
        "materializing source",
    );

    let staged = match tag.as_str() {
        "Http" => {
            let origin_object = origin_value
                .as_object()
                .cloned()
                .ok_or_else(|| format!("source '{}': origin: expected object", entry.name))?;
            let origin = http::parse_http_origin(origin_object, "origin")
                .map_err(|error| format!("source '{}': {error}", entry.name))?;
            fetch_http_source(engine, &origin, &workspace, &subject_logger).await
        }
        "OciRegistry" => fetch_oci_source(engine, &origin_value, &workspace, &subject_logger).await,
        other => Err(format!(
            "source '{}': origin tag '{other}' is not supported by bobr-fetch",
            entry.name
        )),
    };
    let staged = match staged {
        Ok(staged) => staged,
        Err(message) => {
            let message = reclassified(engine, message);
            log_subject_error(&subject_logger, &message);
            return Err(message);
        }
    };

    // Import through the same store code the build uses: canonical timestamps,
    // hashing, refs. A mismatch still imports -- that is the placeholder
    // cycle's contract -- and is reported at the end of the run in one batch.
    let outcome = {
        let engine = engine.clone();
        let name = entry.name.clone();
        run_blocking(move || {
            import_source_object(&engine.store, declared, &staged, &name, engine.run.run_id())
                .map_err(|error| error.to_string())
        })
        .await??
    };
    match outcome {
        SourceImportOutcome::Matched(object_hash) => {
            let _ = engine.run.remove_scratch(workspace.temp_dir());
            subject_logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::Done,
                op: None,
                message: "source fetched and imported".to_string(),
                object_hash: Some(object_hash),
                raw_log_path: None,
                details: Map::new(),
            });
            Ok(SourceOutcome::Downloaded)
        }
        SourceImportOutcome::Mismatched { actual_hash } => {
            let message = format!(
                "source '{}' materialized unexpected object hash: expected {}, got {}",
                entry.name, declared, actual_hash
            );
            log_subject_error(&subject_logger, &message);
            Ok(SourceOutcome::Mismatched(Mismatch {
                name: entry.name,
                declared: declared.to_string(),
                actual: actual_hash.to_string(),
            }))
        }
    }
}

/// A cancel mid-download surfaces as an opaque transport error; name it for
/// what it was so the run does not read as a network failure.
fn reclassified(engine: &Engine, message: String) -> String {
    if engine.is_cancelled() {
        "cancelled".to_string()
    } else {
        message
    }
}

// ---------------------------------------------------------------------------
// Http
// ---------------------------------------------------------------------------

async fn fetch_http_source(
    engine: &Arc<Engine>,
    origin: &HttpOrigin,
    workspace: &Workspace,
    logger: &Arc<dyn BuildLogger>,
) -> Result<PathBuf, String> {
    let blob = download_first_success(engine, &origin.urls, workspace.temp_dir(), logger)
        .await
        .map_err(|error| error.to_string())?;
    let temp_root = workspace.temp_dir().to_path_buf();
    let origin = origin.clone();
    run_blocking(move || {
        http::finalize_http_download(&temp_root, blob, &origin).map_err(|error| error.to_string())
    })
    .await?
}

/// The same two-pass mirror walk as the synchronous path: one attempt per URL
/// first -- a mirror list is what covers a host being down, and it should be
/// consulted before a retry budget is spent -- then the remaining attempts on
/// whatever failed transiently.
async fn download_first_success(
    engine: &Arc<Engine>,
    urls: &[String],
    temp_dir: &Path,
    logger: &Arc<dyn BuildLogger>,
) -> Result<PathBuf, HttpOriginError> {
    let download_path = temp_dir.join("download.blob");
    if download_path.exists() {
        fs::remove_file(&download_path).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to remove stale download '{}': {error}",
                download_path.display()
            ))
        })?;
    }

    let policy = HttpRetryPolicy::production();
    let mut state: Vec<UrlAttemptState> = urls.iter().map(|_| UrlAttemptState::default()).collect();

    for (index, url) in urls.iter().enumerate() {
        if engine.is_cancelled() {
            return Err(cancelled_error());
        }
        match download_with_attempts(engine, url, &download_path, logger, policy, 1, 0).await {
            Ok(()) => return Ok(download_path),
            Err((error, attempts)) => {
                state[index].record(&error, attempts);
                let _ = fs::remove_file(&download_path);
            }
        }
    }

    let remaining = policy.attempts.saturating_sub(1);
    if remaining > 0 {
        for (index, url) in urls.iter().enumerate() {
            if !state[index].worth_retrying {
                continue;
            }
            if engine.is_cancelled() {
                return Err(cancelled_error());
            }
            let spent = state[index].attempts;
            match download_with_attempts(
                engine,
                url,
                &download_path,
                logger,
                policy,
                remaining,
                spent,
            )
            .await
            {
                Ok(()) => return Ok(download_path),
                Err((error, attempts)) => {
                    state[index].record(&error, attempts);
                    let _ = fs::remove_file(&download_path);
                }
            }
        }
    }

    let failures: Vec<String> = urls
        .iter()
        .zip(&state)
        .map(|(url, state)| state.describe(url))
        .collect();
    Err(HttpOriginError::fatal_network(format!(
        "all download URLs failed:\n  - {}",
        failures.join("\n  - ")
    )))
}

async fn download_with_attempts(
    engine: &Arc<Engine>,
    url: &str,
    destination: &Path,
    logger: &Arc<dyn BuildLogger>,
    policy: HttpRetryPolicy,
    attempts_here: u32,
    spent_before: u32,
) -> Result<(), (HttpOriginError, u32)> {
    for attempt in 1..=attempts_here {
        let overall = spent_before + attempt;
        let error = match download_once(engine, url, destination, logger).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Retry::After(retry_after) = error.retry() else {
            return Err((error, attempt));
        };
        if attempt == attempts_here {
            return Err((error, attempt));
        }
        let _ = fs::remove_file(destination);
        let delay = policy.delay_before(overall + 1, retry_after, url);
        // The same fields the synchronous path emits: the run summary counts
        // retries per host from them.
        let mut details = Map::new();
        details.insert(
            "retry_host".to_string(),
            Value::String(http::url_host(url).to_string()),
        );
        details.insert("attempt".to_string(), Value::Number((overall + 1).into()));
        logger.log_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::Running,
            op: Some("fetch".to_string()),
            message: format!(
                "retrying {url} in {:.1}s (attempt {} of {}): {error}",
                delay.as_secs_f64(),
                overall + 1,
                policy.attempts
            ),
            object_hash: None,
            raw_log_path: None,
            details,
        });
        tokio::select! {
            _ = engine.until_cancelled() => return Err((cancelled_error(), attempt)),
            _ = tokio::time::sleep(delay) => {}
        }
    }
    unreachable!("the loop returns on the last attempt")
}

/// One attempt at one URL. The connection limits are taken here -- per
/// attempt, not per URL -- so a backoff pause does not hold a slot on a host
/// that other downloads are waiting for.
async fn download_once(
    engine: &Arc<Engine>,
    url: &str,
    destination: &Path,
    logger: &Arc<dyn BuildLogger>,
) -> Result<(), HttpOriginError> {
    let host = http::url_host(url).to_string();
    let _permits = engine.acquire_permits(&host).await?;
    log_download(
        logger,
        BuildLogLevel::Info,
        &host,
        Some(0),
        None,
        format!("fetching {url}"),
    );

    let response = tokio::select! {
        _ = engine.until_cancelled() => return Err(cancelled_error()),
        response = engine.client.get(url).send() => response,
    };
    let response = response.map_err(|error| {
        if error.is_timeout() {
            HttpOriginError::transient_network(
                format!(
                    "download timed out while requesting '{url}': {}",
                    error_with_causes(&error)
                ),
                None,
            )
        } else {
            HttpOriginError::transient_network(
                format!("failed to download '{url}': {}", error_with_causes(&error)),
                None,
            )
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        let message = format!("failed to download '{url}': HTTP {status}");
        // Same reading as the synchronous path: 5xx is the server's own
        // fault and 429 asks for a pause; both are worth another attempt.
        // Every other 4xx says the file is not there.
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(http::parse_retry_after);
        return Err(
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                HttpOriginError::transient_network(message, retry_after)
            } else {
                HttpOriginError::fatal_network(message)
            },
        );
    }

    let total_bytes = response.content_length();
    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to create temporary download file '{}': {error}",
                destination.display()
            ))
        })?;
    let mut response = response;
    let mut downloaded: u64 = 0;
    let mut last_tick = Instant::now();
    loop {
        if engine.is_cancelled() {
            return Err(HttpOriginError::fatal_network(format!(
                "download of '{url}' cancelled"
            )));
        }
        let chunk = response.chunk().await.map_err(|error| {
            HttpOriginError::transient_network(
                format!(
                    "failed to read HTTP response body from '{url}': {}",
                    error_with_causes(&error)
                ),
                None,
            )
        })?;
        let Some(bytes) = chunk else { break };
        file.write_all(&bytes).await.map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to write temporary download file '{}': {error}",
                destination.display()
            ))
        })?;
        downloaded += bytes.len() as u64;
        if last_tick.elapsed() >= Duration::from_secs(1) {
            log_download(
                logger,
                BuildLogLevel::Progress,
                &host,
                Some(downloaded),
                total_bytes,
                format!("downloaded {downloaded} bytes from {url}"),
            );
            last_tick = Instant::now();
        }
    }
    file.flush().await.map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to write temporary download file '{}': {error}",
            destination.display()
        ))
    })?;
    log_download(
        logger,
        BuildLogLevel::Info,
        &host,
        Some(downloaded),
        total_bytes,
        format!("fetched {downloaded} bytes from {url}"),
    );
    Ok(())
}

/// The error and everything behind it. reqwest's own Display stops at
/// "error decoding response body", which reads as a payload problem while the
/// cause underneath is a timeout or a reset -- exactly the part a diagnosis
/// needs.
fn error_with_causes(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn http_client(timeouts: HttpTimeouts) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(http::REDIRECT_LIMIT))
        .user_agent(http::USER_AGENT)
        .connect_timeout(timeouts.connect)
        // Between reads, not for the whole request: a whole-request budget of
        // sixty seconds means a file that takes longer than a minute to
        // download can never be fetched at all -- every attempt died at
        // exactly 60.0s on the first from-scratch run over a home link. What
        // the timeout is for is a stalled body (a host that accepts and then
        // serves nothing), and that is what a read timeout catches.
        .read_timeout(timeouts.operation)
        // Idle kept-alive sockets are file descriptors too; hundreds of
        // sources across dozens of hosts add up. The active side is bounded by
        // the semaphores, this bounds what lingers afterwards.
        .pool_max_idle_per_host(2)
        .build()
        .map_err(|error| format!("failed to create HTTP client: {error}"))
}

// ---------------------------------------------------------------------------
// OciRegistry
// ---------------------------------------------------------------------------

/// OCI is fetched by the existing synchronous client on a blocking thread: the
/// protocol part (bearer tokens, manifest selection, layer walk) is not worth
/// rewriting for this step. The permits are taken for the registry host and
/// held for the whole pull -- coarser than per-blob, and accepted as such.
async fn fetch_oci_source(
    engine: &Arc<Engine>,
    origin_value: &Value,
    workspace: &Workspace,
    logger: &Arc<dyn BuildLogger>,
) -> Result<PathBuf, String> {
    let image_host = origin_value
        .get("image")
        .and_then(Value::as_str)
        .and_then(|image| image.split('/').next())
        .unwrap_or("oci-registry")
        .to_string();
    let parsed = crate::origins::parse_origin_value(origin_value.clone(), "origin")
        .map_err(|error| error.to_string())?;

    let _permits = engine
        .acquire_permits(&image_host)
        .await
        .map_err(|error| error.to_string())?;

    let temp_root = workspace.temp_dir().to_path_buf();
    let engine = engine.clone();
    let logger = logger.clone();
    run_blocking(move || {
        let cx = OriginContext {
            temp_root: &temp_root,
            logger: logger.as_ref(),
            cancellation: &engine.cancellation,
        };
        parsed.materialize(&cx)
    })
    .await?
}

// ---------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------

/// The store publishes staged files by renaming and hardlinking; neither
/// crosses a filesystem boundary, and the failure would otherwise land
/// mid-fetch on the first import rather than here.
fn check_same_filesystem(store: &Store, run: &Run) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let device_of = |path: &Path, label: &str| -> Result<u64, String> {
        fs::metadata(path)
            .map(|metadata| metadata.dev())
            .map_err(|error| {
                format!(
                    "failed to inspect the {label} '{}': {error}",
                    path.display()
                )
            })
    };
    let store_device = device_of(store.root(), "store")?;
    let work_device = device_of(run.work_dir(), "run work directory")?;
    if store_device == work_device {
        return Ok(());
    }
    Err(format!(
        "run work directory '{}' is on a different filesystem than the store '{}'; \
         downloads stage there and the store publishes them by renaming and hardlinking, \
         which cannot cross a filesystem boundary",
        run.work_dir().display(),
        store.root().display()
    ))
}

async fn run_blocking<T, F>(function: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(function)
        .await
        .map_err(|error| format!("blocking task panicked: {error}"))
}

fn run_event(
    level: BuildLogLevel,
    status: BuildStatus,
    message: String,
    object_hash: Option<ObjectHash>,
) -> BuildLogEvent {
    BuildLogEvent {
        level,
        status,
        op: None,
        message,
        object_hash,
        raw_log_path: None,
        details: Map::new(),
    }
}

/// Where a source means to go, from its origin: the first mirror for HTTP, the
/// registry for OCI. Only ever a hint -- the fetch milestone reports the host
/// actually reached, which differs once a later mirror is taken.
fn intended_host(origin: &Value) -> String {
    match origin.get("tag").and_then(Value::as_str) {
        Some("Http") => origin
            .get("url")
            .and_then(|urls| match urls {
                Value::Array(list) => list.first().and_then(Value::as_str),
                other => other.as_str(),
            })
            .map(|url| http::url_host(url).to_string())
            .unwrap_or_else(|| "?".to_string()),
        Some("OciRegistry") => origin
            .get("image")
            .and_then(Value::as_str)
            .and_then(|image| image.split('/').next())
            .unwrap_or("oci-registry")
            .to_string(),
        _ => "?".to_string(),
    }
}

fn host_details(host: &str) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("host".to_string(), Value::String(host.to_string()));
    details
}

fn log_subject(logger: &Arc<dyn BuildLogger>, status: BuildStatus, message: &str) {
    logger.log_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        status,
        op: None,
        message: message.to_string(),
        object_hash: None,
        raw_log_path: None,
        details: Map::new(),
    });
}

fn log_subject_error(logger: &Arc<dyn BuildLogger>, message: &str) {
    logger.log_event(BuildLogEvent {
        level: BuildLogLevel::Error,
        status: BuildStatus::Failed,
        op: None,
        message: message.to_string(),
        object_hash: None,
        raw_log_path: None,
        details: Map::new(),
    });
}

/// A download milestone or tick. The host and the byte counts travel as fields
/// rather than only inside the sentence: the live log adds them up, and adding
/// up should not mean parsing prose written for a person.
fn log_download(
    logger: &Arc<dyn BuildLogger>,
    level: BuildLogLevel,
    host: &str,
    bytes: Option<u64>,
    total_bytes: Option<u64>,
    message: String,
) {
    let mut details = host_details(host);
    if let Some(bytes) = bytes {
        details.insert("bytes".to_string(), Value::Number(bytes.into()));
    }
    if let Some(total) = total_bytes {
        details.insert("total_bytes".to_string(), Value::Number(total.into()));
    }
    logger.log_event(BuildLogEvent {
        level,
        status: BuildStatus::Running,
        op: Some("fetch".to_string()),
        message,
        object_hash: None,
        raw_log_path: None,
        details,
    });
}

fn log_run_started(logger: &Arc<BuildRunLogger>, sources: usize, limits: &ResolvedLimits) {
    let details = json!({
        // A line per source would be hundreds of lines that scroll past and
        // take the scrollback with them; ask the live log for the aggregate
        // shape instead (see FetchProgress in bobr-core).
        "progress": "aggregate",
        "sources": sources,
        "per_host_default": limits.per_host_default,
        "max_connections": limits.max_connections,
    });
    let Value::Object(details) = details else {
        unreachable!()
    };
    logger.log_run_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        status: BuildStatus::RunStarted,
        op: None,
        message: format!("fetching {sources} source(s)"),
        object_hash: None,
        raw_log_path: None,
        details,
    });
}

fn log_run_finished(logger: &Arc<BuildRunLogger>, summary: &Summary) {
    let result = if summary.cancelled {
        "cancelled"
    } else if summary.is_success() {
        "ok"
    } else {
        "failed"
    };
    let mut message = format!(
        "fetch finished: {} downloaded · {} already present · {} left to the build",
        summary.downloaded, summary.cache_hit, summary.path_skipped
    );
    // The logger has been counting retries by host all along (the retry
    // milestones carry the host as a field); a run that only succeeded on
    // second tries should not read like one that never stumbled.
    let retries = logger.download_retries();
    if !retries.is_empty() {
        let total: u64 = retries.values().sum();
        let mut hosts: Vec<_> = retries.iter().collect();
        hosts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let by_host = hosts
            .iter()
            .map(|(host, count)| format!("{host} x{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        message.push_str(&format!(" · {total} download retries ({by_host})"));
    }
    if !summary.failed.is_empty() {
        message.push_str(&format!(" · {} failed:", summary.failed.len()));
        for (name, reason) in &summary.failed {
            let first_line = reason.lines().next().unwrap_or(reason);
            message.push_str(&format!(
                "
  {name}: {first_line}"
            ));
        }
    }
    // The batch of real hashes is the point of running the fetcher against a
    // recipe still carrying placeholders: every correction in one place.
    if !summary.mismatched.is_empty() {
        message.push_str(&format!(
            "\n{} source(s) materialized unexpected object hashes:",
            summary.mismatched.len()
        ));
        for mismatch in &summary.mismatched {
            message.push_str(&format!(
                "\n  {}: expected {}, got {}",
                mismatch.name, mismatch.declared, mismatch.actual
            ));
        }
    }
    let details = json!({
        "result": result,
        "downloaded": summary.downloaded,
        "cache_hit": summary.cache_hit,
        "path_skipped": summary.path_skipped,
        "failed": summary
            .failed
            .iter()
            .map(|(name, _)| Value::String(name.clone()))
            .collect::<Vec<_>>(),
        "mismatched": summary
            .mismatched
            .iter()
            .map(|m| json!({"name": m.name, "expected": m.declared, "got": m.actual}))
            .collect::<Vec<_>>(),
        "logging_errors": logger.logging_errors(),
        "download_retries": retries.values().sum::<u64>(),
        "download_retries_by_host": retries,
    });
    let Value::Object(details) = details else {
        unreachable!()
    };
    let level = if result == "ok" {
        BuildLogLevel::Info
    } else {
        BuildLogLevel::Error
    };
    logger.log_run_event(BuildLogEvent {
        level,
        status: BuildStatus::RunFinished,
        op: None,
        message,
        object_hash: None,
        raw_log_path: None,
        details,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::request::Limits;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use tempfile::TempDir;

    /// A one-endpoint HTTP server: fails the first `failures` requests with
    /// `status_line`, then serves `body`. Serves until dropped.
    fn spawn_server(
        failures: usize,
        status_line: &'static str,
        body: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/src.blob", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                drain_request(&mut stream);
                let n = seen.fetch_add(1, Ordering::SeqCst);
                if n < failures {
                    let response =
                        format!("{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    stream.write_all(response.as_bytes()).unwrap();
                    continue;
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                return;
            }
        });
        (url, requests, handle)
    }

    fn drain_request(stream: &mut TcpStream) {
        let mut buf = [0u8; 1024];
        let mut seen = Vec::new();
        loop {
            let read = stream.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..read]);
            if seen.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
    }

    /// A store and run directories on one filesystem, plus a request skeleton.
    fn request_in(temp: &TempDir, sources: Vec<SourceEntry>) -> FetchRequest {
        let store = temp.path().join("store");
        let logs = temp.path().join("logs/run");
        let work = temp.path().join("work/run");
        fs::create_dir_all(&store).unwrap();
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&work).unwrap();
        FetchRequest {
            schema: crate::fetch::request::FETCH_REQUEST_SCHEMA.to_string(),
            store,
            logs,
            work,
            run_id: "260809120000".to_string(),
            limits: Limits::default(),
            sources,
        }
    }

    fn http_source(name: &str, hash: ObjectHash, urls: &[&str]) -> SourceEntry {
        SourceEntry {
            name: name.to_string(),
            object_hash: hash.to_string(),
            origin: Some(json!({ "tag": "Http", "url": urls })),
        }
    }

    fn declared_for(payload: &[u8]) -> ObjectHash {
        // A downloaded blob is imported as a plain non-executable file, so its
        // object hash is the file-object hash of those bytes.
        fsobj_hash::hash_file_bytes(false, payload)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_a_source_and_a_mirror_covers_a_dead_host() {
        let payload = b"source payload\n".to_vec();
        let declared = declared_for(&payload);
        let (good_url, _, good_handle) = spawn_server(0, "", payload.clone());
        let (bad_url, bad_requests, bad_handle) =
            spawn_server(usize::MAX, "HTTP/1.1 503 Service Unavailable", Vec::new());

        let temp = tempfile::tempdir().unwrap();
        let request = request_in(
            &temp,
            vec![http_source("demo-src", declared, &[&bad_url, &good_url])],
        );
        let store_root = request.store.clone();
        let summary = run_fetch(request).await.unwrap();

        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.downloaded, 1);
        // The object is in the store under the declared hash, imported for real.
        let object = store_root.join("objects").join(declared.to_string());
        assert_eq!(fs::read(object).unwrap(), payload);
        // The mirror answered before the dead host's retry budget was spent.
        assert_eq!(bad_requests.load(Ordering::SeqCst), 1);
        drop(bad_handle);
        good_handle.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_present_object_is_recorded_without_touching_the_network() {
        let payload = b"already here\n".to_vec();
        let declared = declared_for(&payload);

        let temp = tempfile::tempdir().unwrap();
        // URLs point at a closed port: any network attempt would fail loudly,
        // which is exactly what proves none was made.
        let request = request_in(
            &temp,
            vec![http_source(
                "warm-src",
                declared,
                &["http://127.0.0.1:1/nothing"],
            )],
        );
        let store_root = request.store.clone();

        // Warm the store by fetching once from a real server.
        let (url, _, handle) = spawn_server(0, "", payload.clone());
        let warmup = FetchRequest {
            sources: vec![http_source("warm-src", declared, &[&url])],
            run_id: "260809120001".to_string(),
            logs: {
                let dir = temp.path().join("logs/warmup");
                fs::create_dir_all(&dir).unwrap();
                dir
            },
            work: {
                let dir = temp.path().join("work/warmup");
                fs::create_dir_all(&dir).unwrap();
                dir
            },
            schema: crate::fetch::request::FETCH_REQUEST_SCHEMA.to_string(),
            store: store_root.clone(),
            limits: Limits::default(),
        };
        assert!(run_fetch(warmup).await.unwrap().is_success());
        handle.join().unwrap();

        let summary = run_fetch(request).await.unwrap();
        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.cache_hit, 1);
        assert_eq!(summary.downloaded, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mismatch_is_imported_reported_and_does_not_stop_the_run() {
        let payload = b"the real content\n".to_vec();
        let actual = declared_for(&payload);
        // The recipe still carries a placeholder.
        let placeholder = ObjectHash::from_str(
            "00000000000000000000000000000000000000000000000000000000000000aa",
        )
        .unwrap();
        let other_payload = b"a neighbour\n".to_vec();
        let other_declared = declared_for(&other_payload);

        let (url_a, _, handle_a) = spawn_server(0, "", payload.clone());
        let (url_b, _, handle_b) = spawn_server(0, "", other_payload.clone());
        let temp = tempfile::tempdir().unwrap();
        let request = request_in(
            &temp,
            vec![
                http_source("stub-src", placeholder, &[&url_a]),
                http_source("fine-src", other_declared, &[&url_b]),
            ],
        );
        let store_root = request.store.clone();
        let summary = run_fetch(request).await.unwrap();

        // The healthy neighbour finished, the mismatch is one entry with the
        // real hash -- the batch the placeholder cycle runs on.
        assert!(!summary.is_success());
        assert_eq!(summary.downloaded, 1, "{summary:?}");
        assert_eq!(summary.mismatched.len(), 1);
        assert_eq!(summary.mismatched[0].name, "stub-src");
        assert_eq!(summary.mismatched[0].actual, actual.to_string());
        // Imported under its real hash despite the mismatch: the corrected
        // recipe will not download again.
        assert!(
            store_root
                .join("objects")
                .join(actual.to_string())
                .is_file()
        );
        handle_a.join().unwrap();
        handle_b.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_origin_is_left_to_the_build() {
        let temp = tempfile::tempdir().unwrap();
        let declared = declared_for(b"never fetched");
        let request = request_in(
            &temp,
            vec![SourceEntry {
                name: "recipe-script".to_string(),
                // Real lowerings carry a trailing newline here: lock files are
                // imported as text. The fetcher must trim, as the build does.
                object_hash: format!("{declared}\n"),
                origin: Some(json!({ "tag": "Path", "path": "scripts/build.sh" })),
            }],
        );
        let summary = run_fetch(request).await.unwrap();
        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.path_skipped, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_without_origin_or_object_names_the_problem() {
        let temp = tempfile::tempdir().unwrap();
        let declared = declared_for(b"absent");
        let request = request_in(
            &temp,
            vec![SourceEntry {
                name: "orphan-src".to_string(),
                object_hash: declared.to_string(),
                origin: None,
            }],
        );
        let summary = run_fetch(request).await.unwrap();
        assert!(!summary.is_success());
        assert_eq!(summary.failed.len(), 1);
        assert!(summary.failed[0].1.contains("has no origin"), "{summary:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_hashes_are_fetched_once() {
        let payload = b"shared by two nodes\n".to_vec();
        let declared = declared_for(&payload);
        let (url, requests, handle) = spawn_server(0, "", payload.clone());
        let temp = tempfile::tempdir().unwrap();
        let request = request_in(
            &temp,
            vec![
                http_source("shared-a", declared, &[&url]),
                http_source("shared-b", declared, &[&url]),
            ],
        );
        let summary = run_fetch(request).await.unwrap();
        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        handle.join().unwrap();
    }
}
