use crate::origin::{OriginContext, OriginHandler, OriginSpec, ParsedOrigin};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use tar::Archive;
use xz2::read::XzDecoder;
use zip::read::ZipArchive;

const REDIRECT_LIMIT: usize = 10;
const USER_AGENT: &str = "bobr-source-http/0.1";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Attempts per URL, including the first.
const HTTP_RETRY_ATTEMPTS: u32 = 4;
const HTTP_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
/// Also the ceiling for a server-sent Retry-After: a mirror asking us to come
/// back in an hour should not hold a build hostage.
const HTTP_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
/// How much a wait may be stretched to spread simultaneous retries out.
const HTTP_RETRY_JITTER: f64 = 0.5;
/// How often a wait between attempts checks for cancellation.
const HTTP_RETRY_POLL: Duration = Duration::from_millis(100);

static HTTP_ORIGIN_SPEC: OriginSpec = OriginSpec { tag: "Http" };

/// Whether trying the very same URL again could plausibly succeed.
///
/// This has to be carried by the error rather than recovered from its text: a
/// 404 and a 502 are both "the download failed", and only one of them is worth
/// waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Hopeless: the next attempt would fail the same way.
    Never,
    /// Worth retrying, after the server's own Retry-After when it sent one.
    After(Option<Duration>),
}

#[derive(Debug)]
enum HttpOriginError {
    InvalidConfig(String),
    NetworkFailed { message: String, retry: Retry },
    ExtractFailed(String),
    FsFailed(String),
}

impl HttpOriginError {
    /// A transport or server failure that another attempt will not fix.
    fn fatal_network(message: impl Into<String>) -> Self {
        Self::NetworkFailed {
            message: message.into(),
            retry: Retry::Never,
        }
    }

    /// A transport or server failure worth another attempt.
    fn transient_network(message: impl Into<String>, after: Option<Duration>) -> Self {
        Self::NetworkFailed {
            message: message.into(),
            retry: Retry::After(after),
        }
    }

    fn retry(&self) -> Retry {
        match self {
            Self::NetworkFailed { retry, .. } => *retry,
            // A bad recipe, a full disk or an unreadable archive are all as
            // broken on the second attempt as on the first.
            _ => Retry::Never,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::NetworkFailed { message, .. }
            | Self::ExtractFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for HttpOriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type HResult<T> = Result<T, HttpOriginError>;

#[derive(Debug, Clone, Copy)]
struct HttpTimeouts {
    connect: Duration,
    operation: Duration,
}

impl HttpTimeouts {
    fn production() -> Self {
        Self {
            connect: HTTP_CONNECT_TIMEOUT,
            operation: HTTP_OPERATION_TIMEOUT,
        }
    }
}

/// How hard to try one URL before moving to the next.
///
/// Deliberately small. The point is to survive the CDN hiccup that a second
/// request would not have noticed, not to wait out a real outage: the mirror
/// list is what covers a host being down. Four attempts spaced 1s, 2s, 4s cost
/// at most seven seconds per URL, which is nothing against a build measured in
/// hours, and the whole point is that a build measured in hours should not die
/// of a single 500.
#[derive(Debug, Clone, Copy)]
struct HttpRetryPolicy {
    attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    /// How far past the backoff a wait may be stretched, as a fraction of it.
    /// Zero makes delays exact, which is what the tests want.
    jitter: f64,
}

impl HttpRetryPolicy {
    fn production() -> Self {
        Self {
            attempts: HTTP_RETRY_ATTEMPTS,
            base_delay: HTTP_RETRY_BASE_DELAY,
            max_delay: HTTP_RETRY_MAX_DELAY,
            jitter: HTTP_RETRY_JITTER,
        }
    }

    /// Delay before attempt `attempt` (1-based), doubling from the base and
    /// capped. `retry_after` from the server wins when it asked for longer.
    ///
    /// `url` only ever lengthens the wait, and only by a fraction of it. Two
    /// downloads knocked out by the same event -- ten of them hitting one host
    /// at once, say -- would otherwise come back in lockstep and recreate the
    /// pile-up that felled them, since the backoff alone is the same number for
    /// everyone. Spreading them is the whole point; never shortening the wait
    /// keeps a server's own Retry-After from being undercut.
    fn delay_before(&self, attempt: u32, retry_after: Option<Duration>, url: &str) -> Duration {
        let backoff = self
            .base_delay
            .saturating_mul(1_u32 << (attempt.saturating_sub(2)).min(16))
            .min(self.max_delay);
        let delay = match retry_after {
            Some(server) if server > backoff => server.min(self.max_delay),
            _ => backoff,
        };
        delay.saturating_add(self.jitter_for(delay, attempt, url))
    }

    /// A per-URL offset in `[0, delay * jitter)`.
    ///
    /// Derived from the URL and the attempt rather than drawn at random: it
    /// needs to differ between concurrent downloads, not to be unpredictable,
    /// and deriving it keeps a build's timing reproducible and the crate free of
    /// a random-number dependency. Two nodes fetching the identical URL land on
    /// the same offset, which is harmless -- they are one request's worth of
    /// load, not a herd.
    fn jitter_for(&self, delay: Duration, attempt: u32, url: &str) -> Duration {
        if self.jitter <= 0.0 || delay.is_zero() {
            return Duration::ZERO;
        }
        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        attempt.hash(&mut hasher);
        // Map the hash onto [0, 1) and scale it by the allowed spread.
        let fraction = (hasher.finish() >> 11) as f64 / (1_u64 << 53) as f64;
        delay.mul_f64(self.jitter * fraction)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArchiveFormat {
    TarGz,
    TarXz,
    TarBz2,
    Zip,
}

#[derive(Debug, Clone)]
enum UrlField {
    One(String),
    Many(Vec<String>),
}

impl UrlField {
    fn into_list(self) -> Vec<String> {
        match self {
            Self::One(url) => vec![url],
            Self::Many(urls) => urls,
        }
    }
}

#[derive(Debug)]
pub(crate) struct HttpOriginHandler;

#[derive(Debug, Clone)]
struct HttpOrigin {
    urls: Vec<String>,
    unpack: bool,
    archive_format: Option<ArchiveFormat>,
}

impl OriginHandler for HttpOriginHandler {
    fn spec(&self) -> &'static OriginSpec {
        &HTTP_ORIGIN_SPEC
    }

    fn parse(
        &self,
        mut object: Map<String, Value>,
        field_path: &str,
    ) -> Result<Box<dyn ParsedOrigin>, String> {
        let kind = take_string(&mut object, field_path, "tag")?;
        debug_assert_eq!(kind, "Http");
        let urls = take_url_field(&mut object, field_path, "url")?.into_list();
        if urls.is_empty() {
            return Err(format!("{field_path}.url: url list must not be empty"));
        }
        for url in &urls {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!(
                    "{field_path}.url: url '{url}' must start with http:// or https://"
                ));
            }
        }
        let unpack = take_optional_bool(&mut object, field_path, "unpack")?.unwrap_or(false);
        let archive_format =
            take_optional_archive_format(&mut object, field_path, "archive_format")?;
        if !object.is_empty() {
            return Err(format!(
                "{field_path}: unexpected fields: {}",
                object.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(Box::new(HttpOrigin {
            urls,
            unpack,
            archive_format,
        }))
    }
}

impl ParsedOrigin for HttpOrigin {
    fn spec(&self) -> &'static OriginSpec {
        &HTTP_ORIGIN_SPEC
    }

    fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String> {
        materialize_http_origin(cx, self).map_err(|error| error.to_string())
    }

    fn clone_box(&self) -> Box<dyn ParsedOrigin> {
        Box::new(self.clone())
    }
}

fn materialize_http_origin(cx: &OriginContext<'_>, origin: &HttpOrigin) -> HResult<PathBuf> {
    materialize_http_origin_with_timeouts(
        cx,
        origin,
        HttpTimeouts::production(),
        HttpRetryPolicy::production(),
    )
}

fn materialize_http_origin_with_timeouts(
    cx: &OriginContext<'_>,
    origin: &HttpOrigin,
    timeouts: HttpTimeouts,
    policy: HttpRetryPolicy,
) -> HResult<PathBuf> {
    let client = http_client(timeouts)?;
    let downloaded_blob = download_first_success(cx, &client, &origin.urls, policy)?;
    if !origin.unpack {
        return Ok(downloaded_blob);
    }

    let format = select_archive_format(
        origin.archive_format.as_ref(),
        &downloaded_blob,
        &origin.urls,
    )?;
    let staged_dir = cx.temp_root.join("staged");
    recreate_empty_dir_force(&staged_dir)?;
    extract_archive(&downloaded_blob, format, &staged_dir)?;
    let _ = normalize_extracted_root(&staged_dir)?;
    Ok(staged_dir)
}

fn http_client(timeouts: HttpTimeouts) -> HResult<Client> {
    let client = Client::builder()
        .redirect(Policy::limited(REDIRECT_LIMIT))
        .user_agent(USER_AGENT)
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.operation)
        .build()
        .map_err(|error| {
            HttpOriginError::fatal_network(format!("failed to create HTTP client: {error}"))
        })?;
    Ok(client)
}

fn take_string(object: &mut Map<String, Value>, path: &str, field: &str) -> Result<String, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{path}.{field}: expected string"))
}

fn take_url_field(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<UrlField, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    match value {
        Value::String(url) => Ok(UrlField::One(url)),
        Value::Array(items) => {
            let mut urls = Vec::with_capacity(items.len());
            for item in items {
                let Value::String(url) = item else {
                    return Err(format!(
                        "{path}.{field}: expected string or array of strings"
                    ));
                };
                urls.push(url);
            }
            Ok(UrlField::Many(urls))
        }
        _ => Err(format!(
            "{path}.{field}: expected string or array of strings"
        )),
    }
}

fn take_optional_bool(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = object.remove(field) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("{path}.{field}: expected boolean"))
}

fn take_optional_archive_format(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<Option<ArchiveFormat>, String> {
    let Some(value) = object.remove(field) else {
        return Ok(None);
    };
    let Some(value) = value.as_str() else {
        return Err(format!("{path}.{field}: expected string"));
    };
    match value {
        "tar-gz" => Ok(Some(ArchiveFormat::TarGz)),
        "tar-xz" => Ok(Some(ArchiveFormat::TarXz)),
        "tar-bz2" => Ok(Some(ArchiveFormat::TarBz2)),
        "zip" => Ok(Some(ArchiveFormat::Zip)),
        _ => Err(format!(
            "{path}.{field}: unsupported archive format '{value}'"
        )),
    }
}

fn download_first_success(
    cx: &OriginContext<'_>,
    client: &Client,
    urls: &[String],
    policy: HttpRetryPolicy,
) -> HResult<PathBuf> {
    let download_path = cx.temp_root.join("download.blob");
    if download_path.exists() {
        fs::remove_file(&download_path).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to remove stale download '{}': {error}",
                download_path.display()
            ))
        })?;
    }

    let mut failures = Vec::new();
    for url in urls {
        match download_with_retries(cx, client, url, &download_path, policy) {
            Ok(()) => return Ok(download_path),
            Err((error, attempts)) => {
                let attempted = if attempts == 1 {
                    String::new()
                } else {
                    format!(" (after {attempts} attempts)")
                };
                failures.push(format!("{url}{attempted}: {error}"));
                let _ = fs::remove_file(&download_path);
            }
        }
    }

    Err(HttpOriginError::fatal_network(format!(
        "all download URLs failed:\n  - {}",
        failures.join("\n  - ")
    )))
}

/// Downloads one URL, retrying while the failure looks transient.
///
/// Returns the number of attempts made alongside the last error, so the caller
/// can say how hard it tried rather than leaving "failed" to read as "tried
/// once".
fn download_with_retries(
    cx: &OriginContext<'_>,
    client: &Client,
    url: &str,
    destination: &Path,
    policy: HttpRetryPolicy,
) -> Result<(), (HttpOriginError, u32)> {
    for attempt in 1..=policy.attempts {
        let error = match download_to_file(cx, client, url, destination) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Retry::After(retry_after) = error.retry() else {
            return Err((error, attempt));
        };
        if attempt == policy.attempts {
            return Err((error, attempt));
        }
        // A partial download must not be mistaken for the next attempt's.
        let _ = fs::remove_file(destination);
        let delay = policy.delay_before(attempt + 1, retry_after, url);
        // The host travels as a field, not only inside the sentence: the run
        // summary counts retries per host, and counting should not mean parsing
        // a message written for a human.
        let mut details = Map::new();
        details.insert(
            "retry_host".to_string(),
            Value::String(url_host(url).to_string()),
        );
        details.insert("attempt".to_string(), Value::Number((attempt + 1).into()));
        cx.milestone_with_details(
            format!(
                "retrying {url} in {:.1}s (attempt {} of {}): {error}",
                delay.as_secs_f64(),
                attempt + 1,
                policy.attempts
            ),
            details,
        );
        if let Err(cancelled) = sleep_unless_cancelled(cx, delay) {
            return Err((cancelled, attempt));
        }
    }
    unreachable!("the loop returns on the last attempt")
}

/// The host part of a URL, for grouping retries by who was slow.
fn url_host(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = rest.split('/').next().unwrap_or(rest);
    // Strip userinfo and port, so one host counts as one host.
    let host = host.rsplit('@').next().unwrap_or(host);
    host.split(':').next().unwrap_or(host)
}

/// Waits, giving up early if the run is cancelled.
///
/// Polling rather than one long sleep is what makes Ctrl-C during a backoff
/// feel immediate instead of taking effect once the delay has elapsed.
fn sleep_unless_cancelled(cx: &OriginContext<'_>, delay: Duration) -> Result<(), HttpOriginError> {
    let deadline = Instant::now() + delay;
    loop {
        if cx.is_cancelled() {
            return Err(HttpOriginError::fatal_network(
                "download cancelled while waiting to retry",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(HTTP_RETRY_POLL));
    }
}

fn download_to_file(
    cx: &OriginContext<'_>,
    client: &Client,
    url: &str,
    destination: &Path,
) -> HResult<()> {
    cx.milestone(format!("fetching {url}"));
    let mut response = client
        .get(url)
        .send()
        .map_err(|error| download_request_error(url, error))?;
    let status = response.status();
    if !status.is_success() {
        let message = format!("failed to download '{url}': HTTP {status}");
        // 5xx is the server saying the fault is its own, and 429 is it asking
        // for a pause; both are worth another attempt. Every other 4xx says the
        // file is not there, and waiting will not put it there.
        return Err(
            if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
                HttpOriginError::transient_network(message, retry_after(&response))
            } else {
                HttpOriginError::fatal_network(message)
            },
        );
    }

    let mut file = File::create(destination).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to create temporary download file '{}': {error}",
            destination.display()
        ))
    })?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_tick = Instant::now();
    loop {
        // Poll cancellation each chunk so a long download stops promptly. The
        // opaque error is reclassified to "cancelled" by the source executor.
        if cx.is_cancelled() {
            return Err(HttpOriginError::fatal_network(format!(
                "download of '{url}' cancelled"
            )));
        }
        let read_bytes = response.read(&mut buffer).map_err(|error| {
            // A body cut short is a transport failure like any other.
            HttpOriginError::transient_network(
                format!("failed to read HTTP response body from '{url}': {error}"),
                None,
            )
        })?;
        if read_bytes == 0 {
            break;
        }
        file.write_all(&buffer[..read_bytes]).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to write temporary download file '{}': {error}",
                destination.display()
            ))
        })?;
        downloaded += read_bytes as u64;
        // Throttle transient progress ticks to ~once per second.
        if last_tick.elapsed() >= Duration::from_secs(1) {
            cx.progress(format!("downloaded {downloaded} bytes from {url}"));
            last_tick = Instant::now();
        }
    }
    cx.milestone(format!("fetched {downloaded} bytes from {url}"));
    Ok(())
}

/// The server's Retry-After, when it sent one in seconds.
fn retry_after(response: &reqwest::blocking::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Classifies a failure to even get a response.
///
/// Timeouts, refused connections, resets and DNS failures are all things a
/// second attempt can find fixed, so none of them is treated as final.
fn download_request_error(url: &str, error: reqwest::Error) -> HttpOriginError {
    if error.is_timeout() {
        return HttpOriginError::transient_network(
            format!("download timed out while requesting '{url}': {error}"),
            None,
        );
    }
    HttpOriginError::transient_network(format!("failed to download '{url}': {error}"), None)
}

fn select_archive_format(
    explicit_format: Option<&ArchiveFormat>,
    downloaded_blob: &Path,
    urls: &[String],
) -> HResult<ArchiveFormat> {
    if let Some(format) = explicit_format {
        return Ok(format.clone());
    }
    if let Some(format) = detect_archive_format_from_magic(downloaded_blob)? {
        return Ok(format);
    }
    for url in urls {
        if let Some(format) = detect_archive_format_from_url(url) {
            return Ok(format);
        }
    }
    Err(HttpOriginError::InvalidConfig(format!(
        "unable to detect archive format for URLs {}; set archive_format explicitly or use unpack = false",
        urls.join(", ")
    )))
}

fn detect_archive_format_from_magic(path: &Path) -> HResult<Option<ArchiveFormat>> {
    let mut file = File::open(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to open cached blob for archive detection '{}': {error}",
            path.display()
        ))
    })?;
    let mut header = [0_u8; 8];
    let bytes_read = file.read(&mut header).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to read cached blob for archive detection '{}': {error}",
            path.display()
        ))
    })?;
    let header = &header[..bytes_read];

    if header.len() >= 2 && header[0] == 0x1f && header[1] == 0x8b {
        return Ok(Some(ArchiveFormat::TarGz));
    }
    if header.len() >= 6
        && header[0] == 0xfd
        && header[1] == 0x37
        && header[2] == 0x7a
        && header[3] == 0x58
        && header[4] == 0x5a
        && header[5] == 0x00
    {
        return Ok(Some(ArchiveFormat::TarXz));
    }
    if header.len() >= 3 && header[0] == 0x42 && header[1] == 0x5a && header[2] == 0x68 {
        return Ok(Some(ArchiveFormat::TarBz2));
    }
    if header.len() >= 4
        && header[0] == 0x50
        && header[1] == 0x4b
        && matches!(header[2], 0x03 | 0x05 | 0x07)
        && matches!(header[3], 0x04 | 0x06 | 0x08)
    {
        return Ok(Some(ArchiveFormat::Zip));
    }

    Ok(None)
}

fn detect_archive_format_from_url(url: &str) -> Option<ArchiveFormat> {
    let url_lower = url.to_ascii_lowercase();
    if url_lower.ends_with(".tar.gz") || url_lower.ends_with(".tgz") {
        return Some(ArchiveFormat::TarGz);
    }
    if url_lower.ends_with(".tar.xz") {
        return Some(ArchiveFormat::TarXz);
    }
    if url_lower.ends_with(".tar.bz2")
        || url_lower.ends_with(".tbz2")
        || url_lower.ends_with(".tbz")
    {
        return Some(ArchiveFormat::TarBz2);
    }
    if url_lower.ends_with(".zip") {
        return Some(ArchiveFormat::Zip);
    }
    None
}

fn extract_archive(archive_path: &Path, format: ArchiveFormat, destination: &Path) -> HResult<()> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, destination),
        ArchiveFormat::TarXz => extract_tar_xz(archive_path, destination),
        ArchiveFormat::TarBz2 => extract_tar_bz2(archive_path, destination),
        ArchiveFormat::Zip => extract_zip(archive_path, destination),
    }
}

fn extract_tar_gz(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open tar.gz archive '{}': {error}",
            archive_path.display()
        ))
    })?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn extract_tar_xz(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open tar.xz archive '{}': {error}",
            archive_path.display()
        ))
    })?;
    let decoder = XzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn extract_tar_bz2(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open tar.bz2 archive '{}': {error}",
            archive_path.display()
        ))
    })?;
    let decoder = BzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn unpack_tar_safely<R: Read>(archive: &mut Archive<R>, destination: &Path) -> HResult<()> {
    let entries = archive.entries().map_err(|error| {
        HttpOriginError::ExtractFailed(format!("failed to read tar archive entries: {error}"))
    })?;

    for entry_result in entries {
        let mut entry = entry_result.map_err(|error| {
            HttpOriginError::ExtractFailed(format!("failed to parse tar entry: {error}"))
        })?;

        entry.unpack_in(destination).map_err(|error| {
            HttpOriginError::ExtractFailed(format!(
                "failed to extract tar entry into '{}': {error}",
                destination.display()
            ))
        })?;
    }

    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open zip archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    let mut zip = ZipArchive::new(file).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open zip archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| {
            HttpOriginError::ExtractFailed(format!("failed to read zip entry #{index}: {error}"))
        })?;

        let enclosed = entry.enclosed_name().ok_or_else(|| {
            HttpOriginError::ExtractFailed(format!(
                "zip entry '{}' has invalid or unsafe path",
                entry.name()
            ))
        })?;

        let target_path = destination.join(enclosed);
        if !target_path
            .components()
            .all(|component| !matches!(component, Component::ParentDir | Component::RootDir))
        {
            return Err(HttpOriginError::ExtractFailed(format!(
                "zip entry '{}' resolves to unsafe path",
                entry.name()
            )));
        }

        if entry.is_dir() {
            fs::create_dir_all(&target_path).map_err(|error| {
                HttpOriginError::ExtractFailed(format!(
                    "failed to create directory '{}': {error}",
                    target_path.display()
                ))
            })?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                HttpOriginError::ExtractFailed(format!(
                    "failed to create parent directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }

        let mut out = File::create(&target_path).map_err(|error| {
            HttpOriginError::ExtractFailed(format!(
                "failed to create file '{}': {error}",
                target_path.display()
            ))
        })?;

        std::io::copy(&mut entry, &mut out).map_err(|error| {
            HttpOriginError::ExtractFailed(format!(
                "failed to extract zip entry '{}' to '{}': {error}",
                entry.name(),
                target_path.display()
            ))
        })?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            fs::set_permissions(&target_path, fs::Permissions::from_mode(mode)).map_err(
                |error| {
                    HttpOriginError::ExtractFailed(format!(
                        "failed to set permissions on '{}': {error}",
                        target_path.display()
                    ))
                },
            )?;
        }
    }

    Ok(())
}

fn normalize_extracted_root(directory: &Path) -> HResult<bool> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to read extracted directory '{}': {error}",
                directory.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to list extracted directory '{}': {error}",
                directory.display()
            ))
        })?;

    if entries.len() != 1 {
        return Ok(false);
    }

    let only_entry = entries.remove(0);
    let only_entry_path = only_entry.path();
    let only_entry_file_type = only_entry.file_type().map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to inspect extracted entry '{}': {error}",
            only_entry_path.display()
        ))
    })?;
    if !only_entry_file_type.is_dir() {
        return Ok(false);
    }

    for child in fs::read_dir(&only_entry_path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to read extracted root directory '{}': {error}",
            only_entry_path.display()
        ))
    })? {
        let child = child.map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to list extracted root directory '{}': {error}",
                only_entry_path.display()
            ))
        })?;
        let child_path = child.path();
        let target_path = directory.join(child.file_name());
        fs::rename(&child_path, &target_path).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to normalize extracted root '{}' -> '{}': {error}",
                child_path.display(),
                target_path.display()
            ))
        })?;
    }

    fs::remove_dir(&only_entry_path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to remove extracted wrapper directory '{}': {error}",
            only_entry_path.display()
        ))
    })?;

    Ok(true)
}

fn recreate_empty_dir_force(path: &Path) -> HResult<()> {
    if fs::symlink_metadata(path).is_ok() {
        if path.is_dir() && !path.is_symlink() {
            remove_dir_force(path)?;
        } else {
            fs::remove_file(path).map_err(|error| {
                HttpOriginError::FsFailed(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

fn remove_dir_force(path: &Path) -> HResult<()> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    make_tree_writable(path)?;
    fs::remove_dir_all(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to remove directory '{}': {error}",
            path.display()
        ))
    })
}

fn make_tree_writable(path: &Path) -> HResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to inspect path '{}': {error}",
            path.display()
        ))
    })?;

    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_dir() {
        let mode = metadata.permissions().mode();
        let desired = mode | 0o700;
        if desired != mode {
            fs::set_permissions(path, fs::Permissions::from_mode(desired)).map_err(|error| {
                HttpOriginError::FsFailed(format!(
                    "failed to adjust permissions for '{}': {error}",
                    path.display()
                ))
            })?;
        }

        for entry in fs::read_dir(path).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to read directory '{}': {error}",
                path.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                HttpOriginError::FsFailed(format!(
                    "failed to read directory entry in '{}': {error}",
                    path.display()
                ))
            })?;
            make_tree_writable(&entry.path())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::{
        BuildLogEvent, BuildLogLevel, BuildLogger, CancellationToken, NoopBuildLogger,
    };
    use flate2::Compression;
    use std::io::{Cursor, Read};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use tempfile::tempdir;

    /// Records every emitted event so tests can assert on milestones/progress.
    #[derive(Debug, Default)]
    struct CapturingLogger {
        events: Mutex<Vec<BuildLogEvent>>,
    }

    impl BuildLogger for CapturingLogger {
        fn log_event(&self, event: BuildLogEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn allocate_raw_log_path(&self, _label: &str) -> Result<PathBuf, String> {
            Err("raw logs unused in test".to_string())
        }
    }

    /// Owns a no-op logger and a fresh cancellation token, and lends them as an
    /// `OriginContext` for tests that just need a staging dir.
    struct TestOrigin {
        logger: NoopBuildLogger,
        cancellation: CancellationToken,
    }

    impl TestOrigin {
        fn new() -> Self {
            Self {
                logger: NoopBuildLogger,
                cancellation: CancellationToken::new(),
            }
        }

        fn cx<'a>(&'a self, temp_root: &'a Path) -> OriginContext<'a> {
            OriginContext {
                temp_root,
                logger: &self.logger,
                cancellation: &self.cancellation,
            }
        }
    }

    fn parse_origin(value: Value) -> Result<Box<dyn ParsedOrigin>, String> {
        HttpOriginHandler.parse(value.as_object().unwrap().clone(), "$.origin")
    }

    #[test]
    fn http_download_emits_milestones() {
        let temp = tempdir().unwrap();
        let payload = b"hi there\n".to_vec();
        let (url, handle) = match spawn_http_server(payload, "application/octet-stream") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url,
            "unpack": false
        }))
        .unwrap();

        let logger = CapturingLogger::default();
        let cancellation = CancellationToken::new();
        origin
            .materialize(&OriginContext {
                temp_root: temp.path(),
                logger: &logger,
                cancellation: &cancellation,
            })
            .unwrap();
        handle.join().unwrap();

        let events = logger.events.lock().unwrap();
        // Milestones are durable (Info); start and end are both emitted.
        assert!(
            events
                .iter()
                .any(|e| e.level == BuildLogLevel::Info && e.message.starts_with("fetching")),
            "missing 'fetching' milestone"
        );
        assert!(
            events
                .iter()
                .any(|e| e.level == BuildLogLevel::Info && e.message.starts_with("fetched")),
            "missing 'fetched' milestone"
        );
    }

    #[test]
    fn http_download_aborts_when_cancelled() {
        let temp = tempdir().unwrap();
        let (url, handle) = match spawn_http_server(b"hello\n".to_vec(), "application/octet-stream")
        {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url,
            "unpack": false
        }))
        .unwrap();

        let logger = NoopBuildLogger;
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = origin
            .materialize(&OriginContext {
                temp_root: temp.path(),
                logger: &logger,
                cancellation: &cancellation,
            })
            .unwrap_err();
        // Detach the server thread: the body is already buffered, and the client
        // aborts before reading it.
        drop(handle);
        assert!(error.contains("cancelled"), "{error}");
    }

    /// The production policy with the waiting taken out, so retry behaviour can
    /// be tested without the tests taking seconds to run.
    fn test_retry_policy() -> HttpRetryPolicy {
        HttpRetryPolicy {
            attempts: HTTP_RETRY_ATTEMPTS,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter: 0.0,
        }
    }

    /// A server that answers the first `failures` requests with `status` and
    /// then serves the payload, counting every request it saw.
    fn spawn_flaky_server(
        failures: usize,
        status_line: &'static str,
        body: Vec<u8>,
    ) -> Result<(String, Arc<AtomicUsize>, thread::JoinHandle<()>), std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/payload", addr);
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
                    stream.flush().unwrap();
                    continue;
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
                return;
            }
        });
        Ok((url, requests, handle))
    }

    fn fetch_with_policy(
        temp: &Path,
        url: &str,
        policy: HttpRetryPolicy,
    ) -> (TestOrigin, HResult<PathBuf>) {
        let origin = HttpOrigin {
            urls: vec![url.to_string()],
            unpack: false,
            archive_format: None,
        };
        let test_origin = TestOrigin::new();
        let result = materialize_http_origin_with_timeouts(
            &test_origin.cx(temp),
            &origin,
            HttpTimeouts::production(),
            policy,
        );
        (test_origin, result)
    }

    #[test]
    fn a_server_error_is_retried_on_the_same_url() {
        // The failure this whole thing exists for: one 500 from a CDN killing a
        // build that a second request would have completed.
        let temp = tempdir().unwrap();
        let payload = b"recovered\n".to_vec();
        let (url, requests, handle) =
            match spawn_flaky_server(1, "HTTP/1.1 500 Internal Server Error", payload.clone()) {
                Ok(server) => server,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
                Err(error) => panic!("failed to start test HTTP server: {error}"),
            };

        let (_origin, staged) = fetch_with_policy(temp.path(), &url, test_retry_policy());
        handle.join().unwrap();

        assert_eq!(fs::read(staged.unwrap()).unwrap(), payload);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_missing_file_is_not_retried() {
        // 404 says the file is not there; waiting will not put it there, and a
        // mirror list should move on at once.
        let temp = tempdir().unwrap();
        let (url, requests, handle) =
            match spawn_flaky_server(1, "HTTP/1.1 404 Not Found", b"unused".to_vec()) {
                Ok(server) => server,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
                Err(error) => panic!("failed to start test HTTP server: {error}"),
            };

        let (_origin, result) = fetch_with_policy(temp.path(), &url, test_retry_policy());
        let message = result.unwrap_err().to_string();

        assert_eq!(requests.load(Ordering::SeqCst), 1, "{message}");
        assert!(message.contains("HTTP 404"), "{message}");
        // One attempt is the default reading of "failed"; only a retried URL
        // says how many.
        assert!(!message.contains("attempts"), "{message}");
        drop(handle);
    }

    #[test]
    fn exhausted_attempts_are_reported() {
        let temp = tempdir().unwrap();
        let (url, requests, handle) = match spawn_flaky_server(
            usize::MAX,
            "HTTP/1.1 503 Service Unavailable",
            b"unused".to_vec(),
        ) {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };

        let (_origin, result) = fetch_with_policy(temp.path(), &url, test_retry_policy());
        let message = result.unwrap_err().to_string();

        assert_eq!(
            requests.load(Ordering::SeqCst),
            HTTP_RETRY_ATTEMPTS as usize,
            "{message}"
        );
        assert!(
            message.contains(&format!("after {HTTP_RETRY_ATTEMPTS} attempts")),
            "{message}"
        );
        drop(handle);
    }

    #[test]
    fn cancelling_during_a_backoff_stops_at_once() {
        // The wait is polled rather than slept through, so an interrupt during
        // a backoff takes effect now and not when the delay happens to end.
        let temp = tempdir().unwrap();
        let (url, _requests, handle) = match spawn_flaky_server(
            usize::MAX,
            "HTTP/1.1 503 Service Unavailable",
            b"unused".to_vec(),
        ) {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = HttpOrigin {
            urls: vec![url.clone()],
            unpack: false,
            archive_format: None,
        };
        let test_origin = TestOrigin::new();
        let cancellation = test_origin.cancellation.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            cancellation.cancel();
        });

        let started = Instant::now();
        let error = materialize_http_origin_with_timeouts(
            &test_origin.cx(temp.path()),
            &origin,
            HttpTimeouts::production(),
            HttpRetryPolicy {
                attempts: 3,
                base_delay: Duration::from_secs(30),
                max_delay: Duration::from_secs(30),
                jitter: 0.0,
            },
        )
        .unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            error.to_string().contains("cancelled"),
            "{}",
            error.to_string()
        );
        assert!(elapsed < Duration::from_secs(10), "waited {elapsed:?}");
        drop(handle);
    }

    const URL: &str = "https://example.invalid/payload.tar.xz";

    #[test]
    fn jitter_spreads_simultaneous_retries_without_shortening_them() {
        // Ten downloads knocked out by one event must not come back together:
        // the backoff is the same number for all of them, so only the spread
        // keeps them from recreating the pile-up.
        let policy = HttpRetryPolicy::production();
        let delays: Vec<Duration> = (0..10)
            .map(|n| {
                policy.delay_before(2, None, &format!("https://example.invalid/pkg-{n}.tar.xz"))
            })
            .collect();

        let distinct: std::collections::BTreeSet<_> = delays.iter().collect();
        assert!(distinct.len() >= 8, "{delays:?}");
        for delay in &delays {
            // Never shorter than the backoff, so a Retry-After cannot be
            // undercut, and never more than half again as long.
            assert!(*delay >= HTTP_RETRY_BASE_DELAY, "{delay:?}");
            assert!(*delay <= HTTP_RETRY_BASE_DELAY.mul_f64(1.5), "{delay:?}");
        }
    }

    #[test]
    fn jitter_is_the_same_for_one_url_and_attempt() {
        // Derived, not drawn: a build's timing stays reproducible.
        let policy = HttpRetryPolicy::production();
        assert_eq!(
            policy.delay_before(3, None, URL),
            policy.delay_before(3, None, URL)
        );
        assert_ne!(
            policy.delay_before(2, None, URL),
            policy.delay_before(3, None, URL)
        );
    }

    #[test]
    fn retry_after_is_honoured_but_capped() {
        let policy = HttpRetryPolicy {
            jitter: 0.0,
            ..HttpRetryPolicy::production()
        };
        // Plain backoff doubles from the base.
        assert_eq!(policy.delay_before(2, None, URL), HTTP_RETRY_BASE_DELAY);
        assert_eq!(policy.delay_before(3, None, URL), HTTP_RETRY_BASE_DELAY * 2);
        // A server asking for longer than the backoff gets it ...
        assert_eq!(
            policy.delay_before(2, Some(Duration::from_secs(5)), URL),
            Duration::from_secs(5)
        );
        // ... but not enough to hold a build hostage.
        assert_eq!(
            policy.delay_before(2, Some(Duration::from_secs(3600)), URL),
            HTTP_RETRY_MAX_DELAY
        );
        // And a server asking for less does not shorten the backoff.
        assert_eq!(
            policy.delay_before(3, Some(Duration::from_millis(1)), URL),
            HTTP_RETRY_BASE_DELAY * 2
        );
    }

    fn spawn_http_server(
        body: Vec<u8>,
        content_type: &'static str,
    ) -> Result<(String, thread::JoinHandle<()>), std::io::Error> {
        let listener = (0..10)
            .find_map(|attempt| match TcpListener::bind("127.0.0.1:0") {
                Ok(listener) => Some(Ok(listener)),
                Err(error)
                    if attempt < 9
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrInUse
                        ) =>
                {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
                Err(error) => Some(Err(error)),
            })
            .unwrap_or_else(|| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "failed to bind test HTTP listener",
                ))
            })?;
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/payload", addr);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            drain_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                body.len(),
                content_type
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        });
        Ok((url, handle))
    }

    fn spawn_stalled_body_server(
        stall: Duration,
    ) -> Result<(String, thread::JoinHandle<()>), std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/payload", addr);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            drain_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            thread::sleep(stall);
            let _ = stream.write_all(b"x");
            let _ = stream.flush();
        });
        Ok((url, handle))
    }

    fn spawn_fallback_server(
        ok_body: Vec<u8>,
        content_type: &'static str,
    ) -> Result<((String, String), thread::JoinHandle<()>), std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr().unwrap();
        let bad_url = format!("http://{}/bad", addr);
        let good_url = format!("http://{}/good", addr);
        let handle = thread::spawn(move || {
            // Serve until the good URL has been fetched: how many times the bad
            // one is asked for is up to the retry policy, and the test should
            // not have to know.
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("GET /bad ") {
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                        ok_body.len(),
                        content_type
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.write_all(&ok_body).unwrap();
                    stream.flush().unwrap();
                    return;
                }
                stream.flush().unwrap();
            }
        });
        Ok(((bad_url, good_url), handle))
    }

    fn drain_request(stream: &mut TcpStream) {
        let _ = read_request(stream);
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut buf = [0u8; 1024];
        let mut request = Vec::new();
        loop {
            let read = stream.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    fn tar_gz_with_wrapped_root() -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello archive\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg-1.0/README.txt").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &body[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn parses_valid_http_origin() {
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": ["https://example.invalid/a.tar.gz", "https://example.invalid/b.tar.gz"],
            "unpack": false,
            "archive_format": "zip"
        }))
        .unwrap();
        assert_eq!(origin.spec().tag, "Http");
    }

    #[test]
    fn rejects_invalid_url_shape() {
        let error = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": [1, 2]
        }))
        .unwrap_err();
        assert!(
            error.contains("expected string or array of strings"),
            "{error}"
        );
    }

    #[test]
    fn rejects_non_http_urls() {
        let error = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": "ftp://example.invalid/source.tar.gz"
        }))
        .unwrap_err();
        assert!(
            error.contains("must start with http:// or https://"),
            "{error}"
        );
    }

    #[test]
    fn fallback_url_list_works_in_order() {
        let temp = tempdir().unwrap();
        let payload = b"hello fallback\n".to_vec();
        let ((bad_url, good_url), handle) =
            spawn_fallback_server(payload.clone(), "application/octet-stream").unwrap();
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": [bad_url, good_url],
            "unpack": false
        }))
        .unwrap();
        let test_origin = TestOrigin::new();
        let staged = origin.materialize(&test_origin.cx(temp.path())).unwrap();
        handle.join().unwrap();
        assert_eq!(fs::read(staged).unwrap(), payload);
    }

    #[test]
    fn stalled_http_body_times_out() {
        let temp = tempdir().unwrap();
        let (url, handle) = spawn_stalled_body_server(Duration::from_millis(500)).unwrap();
        let origin = HttpOrigin {
            urls: vec![url.clone()],
            unpack: false,
            archive_format: None,
        };
        let test_origin = TestOrigin::new();
        let error = materialize_http_origin_with_timeouts(
            &test_origin.cx(temp.path()),
            &origin,
            HttpTimeouts {
                connect: Duration::from_millis(100),
                operation: Duration::from_millis(100),
            },
            test_retry_policy(),
        )
        .unwrap_err();
        handle.join().unwrap();
        let message = error.to_string();
        assert!(message.contains("all download URLs failed"), "{message}");
        assert!(message.contains(&url), "{message}");
        assert!(
            message.contains("failed to read HTTP response body") || message.contains("timed out"),
            "{message}"
        );
    }

    #[test]
    fn unpack_false_yields_file_object() {
        let temp = tempdir().unwrap();
        let payload = b"hello file\n".to_vec();
        let (url, handle) = match spawn_http_server(payload.clone(), "application/octet-stream") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url,
            "unpack": false
        }))
        .unwrap();
        let test_origin = TestOrigin::new();
        let staged = origin.materialize(&test_origin.cx(temp.path())).unwrap();
        handle.join().unwrap();
        assert!(staged.is_file());
        assert_eq!(fs::read(staged).unwrap(), payload);
    }

    #[test]
    fn omitted_unpack_yields_file_object() {
        let temp = tempdir().unwrap();
        let payload = b"hello default file\n".to_vec();
        let (url, handle) = match spawn_http_server(payload.clone(), "application/octet-stream") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url
        }))
        .unwrap();
        let test_origin = TestOrigin::new();
        let staged = origin.materialize(&test_origin.cx(temp.path())).unwrap();
        handle.join().unwrap();
        assert!(staged.is_file());
        assert_eq!(fs::read(staged).unwrap(), payload);
    }

    #[test]
    fn unpack_true_yields_unpacked_tree_object() {
        let temp = tempdir().unwrap();
        let payload = tar_gz_with_wrapped_root();
        let (url, handle) = match spawn_http_server(payload, "application/gzip") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url,
            "unpack": true
        }))
        .unwrap();
        let test_origin = TestOrigin::new();
        let staged = origin.materialize(&test_origin.cx(temp.path())).unwrap();
        handle.join().unwrap();
        assert!(staged.is_dir());
        assert_eq!(
            fs::read_to_string(staged.join("README.txt")).unwrap(),
            "hello archive\n"
        );
    }

    #[test]
    fn magic_based_archive_detection_works() {
        let temp = tempdir().unwrap();
        let payload = tar_gz_with_wrapped_root();
        let path = temp.path().join("payload.bin");
        fs::write(&path, payload).unwrap();
        let detected = detect_archive_format_from_magic(&path).unwrap();
        assert_eq!(detected, Some(ArchiveFormat::TarGz));
    }

    #[test]
    fn url_suffix_fallback_detection_works() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("payload.bin");
        fs::write(&path, b"not-an-archive").unwrap();
        let detected = select_archive_format(
            None,
            &path,
            &[String::from("https://example.invalid/source.tar.gz")],
        )
        .unwrap();
        assert_eq!(detected, ArchiveFormat::TarGz);
    }

    #[test]
    fn explicit_archive_format_override_works() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("payload.bin");
        fs::write(&path, b"not-an-archive").unwrap();
        let detected = select_archive_format(
            Some(&ArchiveFormat::Zip),
            &path,
            &[String::from("https://example.invalid/source.tar.gz")],
        )
        .unwrap();
        assert_eq!(detected, ArchiveFormat::Zip);
    }

    #[test]
    fn unpacked_tree_matches_tar_hashing_model() {
        let payload = tar_gz_with_wrapped_root();
        let tree_hash = fsobj_hash::hash_tar_reader(GzDecoder::new(Cursor::new(&payload))).unwrap();
        assert_eq!(tree_hash.to_string().len(), 64);
    }
}
