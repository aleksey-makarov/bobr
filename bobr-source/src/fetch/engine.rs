//! Downloads every source of a fetch request into the store.
//!
//! All sources start at once; what actually runs is governed by two layers of
//! semaphores -- one per host, one per process -- so a saturated mirror stalls
//! only its own queue while everyone else proceeds. Local sources are bounded
//! by a third, separate one: they contend for a disk rather than for sockets.
//! The mirror walk, retry classification and backoff are the same ones the
//! synchronous source path uses; only the transport around them is
//! asynchronous.

use crate::fetch::oci;
use crate::fetch::realizer::SourceRealizer;
use crate::fetch::request::{FetchRequest, ResolvedLimits, SourceEntry};
use crate::http::{
    self, HttpOrigin, HttpOriginError, HttpRetryPolicy, HttpTimeouts, Retry, UrlAttemptState,
};
use crate::origin::OriginContext;
use bobr_core::{
    BuildLogEvent, BuildLogLevel, BuildLogSubject, BuildLogger, BuildRunLogger, BuildStatus,
    CancellationToken, ObjectHash, Run, Workspace,
};
use bobr_store::{
    NamedContentSource, NamedTrustedKeyIndex, SecondaryResolver, SourceImportOutcome, Store,
    import_source_object, record_existing_source_object,
};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// What one run of the fetcher did, and what is left for the caller to fix.
#[derive(Debug, Default)]
pub struct Summary {
    /// Sources downloaded and imported this run.
    pub downloaded: u64,
    /// Sources already present in the store; no network was touched for them.
    pub cache_hit: u64,
    /// Sources materialized from a local path: no network was touched, but
    /// they were read, hashed and imported like any other.
    pub local: u64,
    /// Sources imported from secondary content sources by their declared hash.
    pub secondary: u64,
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
pub(crate) struct Engine {
    store: Store,
    run: Arc<Run>,
    logger: Arc<BuildRunLogger>,
    client: reqwest::Client,
    limits: ResolvedLimits,
    global: Arc<Semaphore>,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Local materialization is bounded separately from the network: it
    /// competes for a disk head, not for sockets, and letting a big image
    /// download hold up a local copy (or the other way round) would be an
    /// accident of sharing one number.
    local: Arc<Semaphore>,
    cancellation: CancellationToken,
    cancel_rx: watch::Receiver<bool>,
    /// Held so `cancel_rx.changed()` cannot resolve by sender-drop; cancelling
    /// goes through [`Engine::cancel`].
    cancel_tx: watch::Sender<bool>,
    secondary: Arc<SecondaryResolver>,
}

pub(crate) fn engine_for_dynamic_realizer(
    store: Store,
    run: Arc<Run>,
    logger: Arc<BuildRunLogger>,
    cancellation: CancellationToken,
    secondary: Arc<SecondaryResolver>,
    limits: crate::fetch::Limits,
) -> Result<Arc<Engine>, String> {
    let limits = ResolvedLimits::from_request(&limits);
    let client = http_client(HttpTimeouts::production())?;
    let (cancel_tx, cancel_rx) = watch::channel(false);
    Ok(Arc::new(Engine {
        store,
        run,
        logger,
        client,
        global: Arc::new(Semaphore::new(limits.max_connections as usize)),
        local: Arc::new(Semaphore::new(limits.max_local_jobs as usize)),
        limits,
        hosts: Mutex::new(HashMap::new()),
        cancellation,
        cancel_rx,
        cancel_tx,
        secondary,
    }))
}

impl Engine {
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
        let _ = self.cancel_tx.send(true);
    }

    pub(super) fn is_cancelled(&self) -> bool {
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

    /// One permit for reading, hashing and copying a local source. Waiting is
    /// interruptible, and counts as no attempt: nothing has been read yet.
    async fn acquire_local_permit(&self) -> Result<OwnedSemaphorePermit, HttpOriginError> {
        tokio::select! {
            _ = self.until_cancelled() => Err(cancelled_error()),
            permit = self.local.clone().acquire_owned() => {
                Ok(permit.expect("local semaphore closed"))
            }
        }
    }
}

fn cancelled_error() -> HttpOriginError {
    HttpOriginError::fatal_network("download cancelled")
}

/// Runs a whole fetch request; the returned [`Summary`] is the exit status in
/// structured form. `Err` is reserved for the run itself being unusable (bad
/// directories, unusable store) -- per-source failures land in the summary.
pub async fn run_fetch(request: FetchRequest) -> Result<Summary, String> {
    run_fetch_with_capabilities(request, Vec::new(), Vec::new()).await
}

pub(crate) async fn run_fetch_with_capabilities(
    request: FetchRequest,
    indexes: Vec<NamedTrustedKeyIndex>,
    sources: Vec<NamedContentSource>,
) -> Result<Summary, String> {
    let store = Store::create(&request.store).map_err(|error| error.to_string())?;
    let run = Arc::new(
        Run::new(request.run_id.clone(), &request.logs, &request.work)
            .map_err(|error| error.to_string())?,
    );
    check_same_filesystem(&store, &run)?;
    let quiet = request.quiet.unwrap_or(false);
    let logger = Arc::new(BuildRunLogger::new(run.logs_dir(), run.run_id(), quiet)?);

    let limits = ResolvedLimits::from_request(&request.limits);
    let client = http_client(HttpTimeouts::production())?;
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let secondary = Arc::new(
        SecondaryResolver::new(store.clone(), request.run_id.clone(), indexes, sources)
            .map_err(|error| error.to_string())?,
    );
    let engine = Arc::new(Engine {
        store,
        run,
        logger: logger.clone(),
        client,
        global: Arc::new(Semaphore::new(limits.max_connections as usize)),
        local: Arc::new(Semaphore::new(limits.max_local_jobs as usize)),
        limits,
        hosts: Mutex::new(HashMap::new()),
        cancellation: CancellationToken::new(),
        cancel_rx,
        cancel_tx,
        secondary,
    });

    let realizer = SourceRealizer::new(engine.clone(), request.sources);
    log_run_started(&logger, realizer.node_count(), &engine.limits);

    {
        let engine = engine.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                engine.cancel();
            }
        });
    }

    let mut summary = Summary::default();
    for outcome in realizer.run().await? {
        match outcome {
            SourceOutcome::Downloaded => summary.downloaded += 1,
            SourceOutcome::CacheHit => summary.cache_hit += 1,
            SourceOutcome::Local => summary.local += 1,
            SourceOutcome::Secondary => summary.secondary += 1,
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

#[derive(Debug)]
pub(crate) enum SourceOutcome {
    Downloaded,
    CacheHit,
    Local,
    Secondary,
    Mismatched(Mismatch),
    Failed { name: String, message: String },
}

pub(crate) async fn process_source(engine: Arc<Engine>, entry: SourceEntry) -> SourceOutcome {
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

    if engine.secondary.has_content_sources() {
        let secondary = {
            let engine = engine.clone();
            run_blocking(move || {
                engine
                    .secondary
                    .ensure_objects(&[declared])
                    .map_err(|error| error.to_string())
            })
            .await??
        };
        let secondary = secondary
            .into_iter()
            .next()
            .expect("one requested secondary object produces one report");
        if secondary.outcome.is_some() {
            let recorded = {
                let engine = engine.clone();
                let name = entry.name.clone();
                run_blocking(move || {
                    record_existing_source_object(
                        &engine.store,
                        declared,
                        &name,
                        engine.run.run_id(),
                    )
                    .map_err(|error| error.to_string())
                })
                .await??
            };
            if recorded.is_none() {
                return Err(format!(
                    "secondary acquisition reported source object '{declared}' ready, but it is absent from the working store"
                ));
            }
            engine.logger.log_run_event(run_event(
                BuildLogLevel::Info,
                BuildStatus::CacheHit,
                format!(
                    "source '{}' imported from secondary content source(s): {}",
                    entry.name,
                    if secondary.content_sources.is_empty() {
                        "working store".to_string()
                    } else {
                        secondary.content_sources.join(", ")
                    }
                ),
                Some(declared),
            ));
            return Ok(SourceOutcome::Secondary);
        }
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
        details: source_start_details(&origin_value),
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
        "Path" => fetch_path_source(engine, &origin_value, &workspace, &subject_logger).await,
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
            Ok(if tag == "Path" {
                SourceOutcome::Local
            } else {
                SourceOutcome::Downloaded
            })
        }
        SourceImportOutcome::Mismatched { actual_hash } => {
            // Where the content came from, for the origins where that is a
            // place someone can go and look: a local source that hashes
            // differently is a file on this disk, and naming it saves the
            // reader a trip through the recipes to find out which.
            let from = origin_location(&origin_value)
                .map(|location| format!(" (from {location})"))
                .unwrap_or_default();
            let message = format!(
                "source '{}'{from} materialized unexpected object hash: expected {}, got {}",
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

pub(super) async fn record_source_aliases(
    engine: Arc<Engine>,
    declared: ObjectHash,
    aliases: Vec<String>,
) -> Result<(), String> {
    if aliases.is_empty() {
        return Ok(());
    }
    run_blocking(move || {
        for name in aliases {
            let recorded =
                record_existing_source_object(&engine.store, declared, &name, engine.run.run_id())
                    .map_err(|error| error.to_string())?;
            if recorded.is_none() {
                return Err(format!(
                    "source alias '{name}' cannot find realized object '{declared}'"
                ));
            }
        }
        Ok(())
    })
    .await?
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
            // Resuming a URL is a retry like any other: the same pause before
            // it, and the same line in the log. Without this the second attempt
            // came instantly and silently -- the pause exists because a host
            // that just failed is unlikely to be ready a millisecond later, and
            // that line is the only place a retry is ever counted.
            let spent = state[index].attempts;
            let delay = policy.delay_before(spent + 1, state[index].retry_after, url);
            let (message, details) = http::retry_notice(
                url,
                delay,
                spent + 1,
                policy.attempts,
                state[index]
                    .last_error
                    .as_deref()
                    .unwrap_or("previous attempt failed"),
            );
            logger.log_event(BuildLogEvent {
                level: BuildLogLevel::Info,
                status: BuildStatus::Running,
                op: Some("fetch".to_string()),
                message,
                object_hash: None,
                raw_log_path: None,
                details,
            });
            tokio::select! {
                _ = engine.until_cancelled() => return Err(cancelled_error()),
                _ = tokio::time::sleep(delay) => {}
            }
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
        let (message, details) =
            http::retry_notice(url, delay, overall + 1, policy.attempts, &error.to_string());
        logger.log_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::Running,
            op: Some("fetch".to_string()),
            message,
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
pub(super) fn error_with_causes(error: &reqwest::Error) -> String {
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

/// An image is one source, and the permits are taken for the registry host and
/// held for the whole pull -- coarser than per-blob, which costs nothing while
/// the blobs are fetched one after another.
///
/// Cancelling is dropping the pull: the future stops wherever it is, mid-layer
/// if that is where it was. That is the whole reason this path is asynchronous
/// -- a blocking pull could only be waited out.
async fn fetch_oci_source(
    engine: &Arc<Engine>,
    origin_value: &Value,
    workspace: &Workspace,
    logger: &Arc<dyn BuildLogger>,
) -> Result<PathBuf, String> {
    let origin = oci::parse_oci_origin(origin_value, "origin")?;
    let _permits = engine
        .acquire_permits(&origin.host())
        .await
        .map_err(|error| error.to_string())?;

    let pull = oci::materialize(
        &engine.client,
        logger,
        HttpRetryPolicy::production(),
        &origin,
        workspace.temp_dir(),
    );
    let staged = tokio::select! {
        _ = engine.until_cancelled() => Err("cancelled".to_string()),
        staged = pull => staged,
    };
    if staged.is_err() {
        // What a dropped pull leaves behind is a partial layer, and a mirror
        // walk deletes its half-written file for the same reason: a cancelled
        // run should not leave hundreds of megabytes lying in the work
        // directory for someone to identify later.
        oci::discard(workspace.temp_dir());
    }
    staged
}

// ---------------------------------------------------------------------------
// Path
// ---------------------------------------------------------------------------

/// Copies (or unpacks) a local path into the workspace, using the same origin
/// code the build uses. There is nothing here worth an asynchronous rewrite --
/// no protocol, no retries, no server to be polite to -- so the work goes to a
/// blocking thread as it stands.
///
/// It is bounded all the same. Fifty-five local sources would otherwise be read
/// and hashed at once, which on a spindle turns sequential reads into seeks and
/// finishes slower than doing them in turn; `max_local_jobs` is that bound, and
/// it is deliberately not the connection limit -- one is a disk, the other a
/// network, and they contend for nothing in common.
async fn fetch_path_source(
    engine: &Arc<Engine>,
    origin_value: &Value,
    workspace: &Workspace,
    logger: &Arc<dyn BuildLogger>,
) -> Result<PathBuf, String> {
    let parsed = crate::origins::parse_origin_value(origin_value.clone(), "origin")
        .map_err(|error| error.to_string())?;
    let _permit = engine
        .acquire_local_permit()
        .await
        .map_err(|error| error.to_string())?;
    // Enough to move the subject out of the queue in the live log. No byte
    // counts: this is a disk, and adding it to a figure read as network
    // throughput would misreport both.
    log_subject_host(logger, LOCAL_HOST, "materializing local source");

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
            .map(oci::registry_host)
            .unwrap_or_else(|| "?".to_string()),
        Some("Path") => LOCAL_HOST.to_string(),
        _ => "?".to_string(),
    }
}

/// What a local source is filed under in the live log, where every other
/// source is filed under the host it came from.
const LOCAL_HOST: &str = "local";

/// Where a source's content came from, when that is a place worth naming in an
/// error. A mirror list is not one -- the URL that answered is already in the
/// log -- so only local paths qualify.
fn origin_location(origin: &Value) -> Option<String> {
    match origin.get("tag").and_then(Value::as_str) {
        Some("Path") => origin
            .get("path")
            .and_then(Value::as_str)
            .map(|path| format!("'{path}'")),
        _ => None,
    }
}

fn host_details(host: &str) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("host".to_string(), Value::String(host.to_string()));
    details
}

fn source_start_details(origin: &Value) -> Map<String, Value> {
    let mut details = host_details(&intended_host(origin));
    let transfer = match origin.get("tag").and_then(Value::as_str) {
        Some("Http" | "OciRegistry") => "network",
        _ => "local",
    };
    details.insert("transfer".to_string(), Value::String(transfer.to_string()));
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

/// A subject milestone that names where the work is happening, which is what
/// moves it from queued to active in the live log.
fn log_subject_host(logger: &Arc<dyn BuildLogger>, host: &str, message: &str) {
    let mut details = host_details(host);
    details.insert(
        "transfer".to_string(),
        Value::String(
            if host == LOCAL_HOST {
                "local"
            } else {
                "network"
            }
            .to_string(),
        ),
    );
    logger.log_event(BuildLogEvent {
        level: BuildLogLevel::Info,
        status: BuildStatus::Running,
        op: Some("fetch".to_string()),
        message: message.to_string(),
        object_hash: None,
        raw_log_path: None,
        details,
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
    details.insert("transfer".to_string(), Value::String("network".to_string()));
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

/// `name xN`, most first, ties by name so a run reads the same twice.
fn by_count(counts: &std::collections::BTreeMap<String, u64>) -> String {
    let mut entries: Vec<_> = counts.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    entries
        .iter()
        .map(|(name, count)| format!("{name} x{count}"))
        .collect::<Vec<_>>()
        .join(", ")
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
        "fetch finished: {} downloaded · {} local · {} secondary · {} already present",
        summary.downloaded, summary.local, summary.secondary, summary.cache_hit
    );
    // The logger has been counting retries by host all along (the retry
    // milestones carry the host as a field); a run that only succeeded on
    // second tries should not read like one that never stumbled.
    let retries = logger.download_retries();
    let reasons = logger.download_retry_reasons();
    if !retries.is_empty() {
        let total: u64 = retries.values().sum();
        // Reasons before hosts: the host list is long by nature -- a
        // from-scratch fetch touches a hundred of them -- and by the time the
        // reader has walked it the shape of the run is lost. What was wrong is
        // the shorter and more useful half. A resolver failing on the fetching
        // machine and one host refusing to serve read alike as a count, and
        // want opposite fixes.
        message.push_str(&format!(
            " · {total} download retries ({}; {})",
            by_count(&reasons),
            by_count(&retries)
        ));
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
        "local": summary.local,
        "secondary": summary.secondary,
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
        "download_retries_by_reason": reasons,
    });
    let Value::Object(details) = details else {
        unreachable!()
    };
    // A run that only got through on second attempts is worth a word even when
    // it succeeded, and `quiet` -- which keeps warnings and errors -- is exactly
    // where that word would otherwise be lost: the retries themselves are
    // routine milestones, and this summary is the only other place they are
    // counted. Naming the host is the point; that is what the retry counter was
    // added for.
    let level = if result != "ok" {
        BuildLogLevel::Error
    } else if retries.is_empty() {
        BuildLogLevel::Info
    } else {
        BuildLogLevel::Warn
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
    use bobr_runtime::runtime_provider::RuntimeProvider;
    use bobr_store::{
        LocalHardlinkContentSource, NamedContentSource, ReadOnlyStore, import_source_object,
    };
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::MetadataExt;
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
        request_in_run(temp, "run", sources)
    }

    /// A second run against the same store needs its own directories: a run id
    /// is claimed by creating them, and claiming one twice is what that is for.
    fn request_in_run(temp: &TempDir, run: &str, sources: Vec<SourceEntry>) -> FetchRequest {
        let store = temp.path().join("store");
        let logs = temp.path().join("logs").join(run);
        let work = temp.path().join("work").join(run);
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
            quiet: None,
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
            quiet: None,
        };
        assert!(run_fetch(warmup).await.unwrap().is_success());
        handle.join().unwrap();

        let summary = run_fetch(request).await.unwrap();
        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.cache_hit, 1);
        assert_eq!(summary.downloaded, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_realizer_imports_known_object_from_secondary_content() {
        let payload = b"secondary source payload\n";
        let declared = declared_for(payload);
        let temp = tempfile::tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        fs::create_dir(&secondary_root).unwrap();
        let secondary_store = Store::create(&secondary_root).unwrap();
        let staged = temp.path().join("secondary-staged");
        fs::write(&staged, payload).unwrap();
        assert!(matches!(
            import_source_object(
                &secondary_store,
                declared,
                &staged,
                "secondary-source",
                "secondary-run"
            )
            .unwrap(),
            SourceImportOutcome::Matched(hash) if hash == declared
        ));
        let request = request_in(
            &temp,
            vec![SourceEntry {
                name: "secondary-source".to_string(),
                object_hash: declared.to_string(),
                origin: None,
            }],
        );
        let working_root = request.store.clone();
        let content = NamedContentSource::new(
            "secondary",
            Arc::new(LocalHardlinkContentSource::with_runtime(
                ReadOnlyStore::open(&secondary_root).unwrap(),
                RuntimeProvider::host(),
            )),
        );

        let summary = run_fetch_with_capabilities(request, Vec::new(), vec![content])
            .await
            .unwrap();

        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.secondary, 1);
        assert_eq!(summary.downloaded, 0);
        assert_eq!(
            fs::metadata(secondary_store.object_path(declared).unwrap().unwrap())
                .unwrap()
                .ino(),
            fs::metadata(working_root.join("objects").join(declared.to_string()))
                .unwrap()
                .ino()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_realizer_deduplicates_content_but_records_every_alias() {
        let payload = b"shared secondary source\n";
        let declared = declared_for(payload);
        let temp = tempfile::tempdir().unwrap();
        let secondary_root = temp.path().join("secondary");
        fs::create_dir(&secondary_root).unwrap();
        let secondary_store = Store::create(&secondary_root).unwrap();
        let staged = temp.path().join("secondary-staged");
        fs::write(&staged, payload).unwrap();
        import_source_object(
            &secondary_store,
            declared,
            &staged,
            "secondary-source",
            "secondary-run",
        )
        .unwrap();
        let request = request_in(
            &temp,
            vec![
                SourceEntry {
                    name: "first-alias".to_string(),
                    object_hash: declared.to_string(),
                    origin: None,
                },
                SourceEntry {
                    name: "second-alias".to_string(),
                    object_hash: declared.to_string(),
                    origin: None,
                },
            ],
        );
        let working_root = request.store.clone();
        let content = NamedContentSource::new(
            "secondary",
            Arc::new(LocalHardlinkContentSource::with_runtime(
                ReadOnlyStore::open(&secondary_root).unwrap(),
                RuntimeProvider::host(),
            )),
        );

        let summary = run_fetch_with_capabilities(request, Vec::new(), vec![content])
            .await
            .unwrap();

        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.secondary, 1);
        assert!(working_root.join("object-refs/first-alias").is_symlink());
        assert!(working_root.join("object-refs/second-alias").is_symlink());
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
    async fn a_quiet_run_still_reports_that_it_needed_retries() {
        // Retries are how a mirror that throttles announces itself, and the
        // summary is the only place they are counted. A quiet run keeps
        // warnings, so the summary has to be one when there were retries --
        // otherwise the very signal the counter exists for is what `quiet`
        // silences.
        let payload = b"recovered\n".to_vec();
        let declared = declared_for(&payload);
        let (url, _, handle) = spawn_server(1, "HTTP/1.1 503 Service Unavailable", payload.clone());

        let temp = tempfile::tempdir().unwrap();
        let mut request = request_in(&temp, vec![http_source("flaky", declared, &[&url])]);
        request.quiet = Some(true);
        let logs = request.logs.clone();
        let summary = run_fetch(request).await.unwrap();
        handle.join().unwrap();
        assert!(summary.is_success(), "{summary:?}");

        let events = fs::read_to_string(logs.join("events.jsonl")).unwrap();
        let finished: Value = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["status"] == "run-finished")
            .expect("the run reported its end");
        assert_eq!(finished["level"], "warn", "{finished}");
        let message = finished["message"].as_str().unwrap();
        assert!(message.contains("download retries"), "{finished}");
        // What was wrong, not only how often: a 503 is the host refusing, and
        // that reads differently from a run whose retries were all DNS.
        assert!(message.contains("http 5xx x1"), "{finished}");
        assert_eq!(
            finished["details"]["download_retries_by_reason"]["http 5xx"],
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retries_of_different_kinds_are_counted_apart() {
        // A host that refuses and a name that will not resolve are one number
        // in the old summary and two problems in reality: one is answered by
        // asking that host for less, the other by fixing the resolver on the
        // machine doing the fetching.
        let payload = b"eventually\n".to_vec();
        let declared = declared_for(&payload);
        let (url, _, handle) = spawn_server(1, "HTTP/1.1 503 Service Unavailable", payload.clone());
        // A name that cannot resolve, tried before the mirror that works.
        let dead = "http://host.invalid.test/src.blob".to_string();

        let temp = tempfile::tempdir().unwrap();
        let request = request_in(&temp, vec![http_source("mixed", declared, &[&dead, &url])]);
        let logs = request.logs.clone();
        let summary = run_fetch(request).await.unwrap();
        handle.join().unwrap();
        assert!(summary.is_success(), "{summary:?}");

        let events = fs::read_to_string(logs.join("events.jsonl")).unwrap();
        let finished: Value = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["status"] == "run-finished")
            .expect("the run reported its end");
        let reasons = &finished["details"]["download_retries_by_reason"];
        assert_eq!(reasons["http 5xx"], 1, "{finished}");
        assert!(reasons["dns"].as_u64().unwrap_or(0) >= 1, "{finished}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_clean_run_says_nothing_worth_a_warning() {
        let payload = b"first try\n".to_vec();
        let declared = declared_for(&payload);
        let (url, _, handle) = spawn_server(0, "", payload.clone());

        let temp = tempfile::tempdir().unwrap();
        let request = request_in(&temp, vec![http_source("easy", declared, &[&url])]);
        let logs = request.logs.clone();
        assert!(run_fetch(request).await.unwrap().is_success());
        handle.join().unwrap();

        let events = fs::read_to_string(logs.join("events.jsonl")).unwrap();
        let finished: Value = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["status"] == "run-finished")
            .unwrap();
        assert_eq!(finished["level"], "info", "{finished}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_source_is_copied_in_and_counted_apart_from_downloads() {
        let temp = tempfile::tempdir().unwrap();
        let payload = b"#!/bin/sh\nexit 0\n".to_vec();
        let script = temp.path().join("build.sh");
        fs::write(&script, &payload).unwrap();
        let declared = declared_for(&payload);

        let request = request_in(
            &temp,
            vec![SourceEntry {
                name: "recipe-script".to_string(),
                // Real lowerings carry a trailing newline here: lock files are
                // imported as text. The fetcher must trim, as the build does.
                object_hash: format!("{declared}\n"),
                origin: Some(json!({ "tag": "Path", "path": script.to_str().unwrap() })),
            }],
        );
        let store = request.store.clone();
        let summary = run_fetch(request).await.unwrap();

        assert!(summary.is_success(), "{summary:?}");
        // Read off a disk, not off a network: counted on its own, so that
        // "downloaded" keeps meaning what it says.
        assert_eq!(summary.local, 1);
        assert_eq!(summary.downloaded, 0);
        assert!(
            store.join("objects").join(declared.to_string()).exists(),
            "the local source should be in the store"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_source_already_in_the_store_is_not_read_again() {
        // The declared hash is the identity, and the store either has that
        // object or it does not; the file on disk is one way of obtaining it,
        // not the authority on what it should be. So a warm store means the
        // path is never even opened -- which is also why this run succeeds
        // with the file deleted.
        let temp = tempfile::tempdir().unwrap();
        let payload = b"#!/bin/sh\nexit 0\n".to_vec();
        let script = temp.path().join("build.sh");
        fs::write(&script, &payload).unwrap();
        let declared = declared_for(&payload);

        let source = || SourceEntry {
            name: "recipe-script".to_string(),
            object_hash: declared.to_string(),
            origin: Some(json!({ "tag": "Path", "path": script.to_str().unwrap() })),
        };
        assert_eq!(
            run_fetch(request_in_run(&temp, "first", vec![source()]))
                .await
                .unwrap()
                .local,
            1
        );

        fs::remove_file(&script).unwrap();
        let again = request_in_run(&temp, "second", vec![source()]);
        let summary = run_fetch(again).await.unwrap();
        assert!(summary.is_success(), "{summary:?}");
        assert_eq!(summary.cache_hit, 1);
        assert_eq!(summary.local, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_source_that_hashes_differently_names_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("build.sh");
        fs::write(&script, b"edited since the lock was written\n").unwrap();
        let declared = declared_for(b"what the lock says\n");

        let request = request_in(
            &temp,
            vec![SourceEntry {
                name: "recipe-script".to_string(),
                object_hash: declared.to_string(),
                origin: Some(json!({ "tag": "Path", "path": script.to_str().unwrap() })),
            }],
        );
        let logs = request.logs.clone();
        let summary = run_fetch(request).await.unwrap();

        assert!(!summary.is_success(), "{summary:?}");
        assert_eq!(summary.mismatched.len(), 1);
        // Which file: the reader should not have to go through the recipes to
        // find out what was hashed.
        let events = fs::read_to_string(logs.join("events.jsonl")).unwrap();
        assert!(
            events.contains(script.to_str().unwrap()),
            "the failure should name the path it read: {events}"
        );
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
