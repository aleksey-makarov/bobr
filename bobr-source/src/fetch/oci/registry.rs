//! Talking to an OCI registry, asynchronously.
//!
//! The protocol is the registry v2 one: fetch a manifest (an index, usually,
//! from which the platform's manifest is selected), then the config blob and
//! every layer, verifying each against the digest that named it. What differs
//! from the synchronous client in `crate::oci_registry` is everything around
//! that protocol, and the differences are the reason this exists:
//!
//! - a pull is a future, so the fetcher cancels one by dropping it, rather than
//!   waiting for a blocking thread that cannot be interrupted;
//! - failures are classified and retried with the same policy the mirror walk
//!   uses, instead of failing the whole image on one 503;
//! - progress carries the host and the byte counts as fields, so an image is
//!   counted by the live log like any other download -- and, uniquely, with an
//!   exact total, since the manifest states every blob's size in advance;
//! - blobs stream to disk while being hashed, instead of being held whole in
//!   memory. A layer is hundreds of megabytes.
//!
//! The bearer token is fetched on the first 401 and then reused for the rest of
//! the pull: one image is one repository, so one token covers it.

use crate::fetch::engine::error_with_causes;
use crate::http::{self, HttpRetryPolicy, Retry};
use bobr_core::oci::{self, MEDIA_TYPE_OCI_MANIFEST, OciDescriptor, OciManifest, OciPlatform};
use bobr_core::{BuildLogEvent, BuildLogLevel, BuildLogger, BuildStatus};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MEDIA_TYPE_DOCKER_MANIFEST_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json, ",
    "application/vnd.oci.image.manifest.v1+json, ",
    "application/vnd.docker.distribution.manifest.list.v2+json, ",
    "application/vnd.docker.distribution.manifest.v2+json"
);

/// How much of a registry's error body is worth quoting back. Enough for the
/// `errors[]` array a registry sends, not enough for an HTML error page to bury
/// the line it appears on.
const ERROR_BODY_LIMIT: usize = 512;

/// Failure of a pull, and whether another attempt could plausibly do better.
#[derive(Debug)]
pub(super) struct RegistryError {
    message: String,
    retry: Retry,
}

impl RegistryError {
    /// A failure the next attempt would reproduce: a malformed manifest, a
    /// missing repository, a pin that no longer resolves.
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: Retry::Never,
        }
    }

    /// A failure worth another attempt: the transport, the server's own 5xx,
    /// or a body that arrived corrupted.
    fn transient(message: impl Into<String>, after: Option<Duration>) -> Self {
        Self {
            message: message.into(),
            retry: Retry::After(after),
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<oci::OciError> for RegistryError {
    fn from(error: oci::OciError) -> Self {
        Self::fatal(error.to_string())
    }
}

/// One pull's conversation with one registry: where it is, who it is talking
/// about, the token it was given, and how much it has received so far.
pub(super) struct Session<'a> {
    client: &'a reqwest::Client,
    logger: &'a Arc<dyn BuildLogger>,
    policy: HttpRetryPolicy,
    scheme: &'static str,
    /// Registry host, as reached and as reported: the semaphore key and the
    /// name the live log groups this download under.
    host: String,
    repository: String,
    /// The tag or digest the image reference named. Only used to resolve what a
    /// tag currently points at, which is the hint a stale pin needs.
    reference: String,
    token: Mutex<Option<String>>,
    /// Bytes received for this image so far, across manifests and blobs. The
    /// live log wants one number per source, and an image is one source.
    received: AtomicU64,
    /// Sum of every blob this image is made of, known once the manifest is
    /// parsed; zero until then.
    total: AtomicU64,
    last_tick: Mutex<Instant>,
}

impl<'a> Session<'a> {
    /// Opens a session for `image`, splitting it into the registry to talk to
    /// and the repository to talk about.
    pub(super) fn new(
        client: &'a reqwest::Client,
        logger: &'a Arc<dyn BuildLogger>,
        policy: HttpRetryPolicy,
        image: &str,
    ) -> Result<Self, RegistryError> {
        let (host, repository, reference) = parse_image_ref(image)?;
        let scheme = scheme_for(&host);
        Ok(Self {
            client,
            logger,
            policy,
            scheme,
            host,
            repository,
            reference,
            token: Mutex::new(None),
            received: AtomicU64::new(0),
            total: AtomicU64::new(0),
            last_tick: Mutex::new(Instant::now()),
        })
    }

    fn manifest_url(&self, reference: &str) -> String {
        format!(
            "{}://{}/v2/{}/manifests/{reference}",
            self.scheme, self.host, self.repository
        )
    }

    fn blob_url(&self, digest: &str) -> String {
        format!(
            "{}://{}/v2/{}/blobs/{digest}",
            self.scheme, self.host, self.repository
        )
    }

    // -- progress ---------------------------------------------------------

    /// A milestone: what the pull is about to do, with the counters attached so
    /// the live log can place the source even before any bytes arrive.
    fn milestone(&self, message: String) {
        self.emit(BuildLogLevel::Info, message);
    }

    /// A tick, at most one a second: the same counters, no line in the journal.
    fn tick(&self, message: impl FnOnce() -> String) {
        {
            let mut last = self.last_tick.lock().expect("tick clock poisoned");
            if last.elapsed() < Duration::from_secs(1) {
                return;
            }
            *last = Instant::now();
        }
        self.emit(BuildLogLevel::Progress, message());
    }

    fn emit(&self, level: BuildLogLevel, message: String) {
        let received = self.received.load(Ordering::Relaxed);
        let total = match self.total.load(Ordering::Relaxed) {
            0 => None,
            total => Some(total),
        };
        let mut details = Map::new();
        details.insert("host".to_string(), Value::String(self.host.clone()));
        details.insert("transfer".to_string(), Value::String("network".to_string()));
        details.insert("bytes".to_string(), Value::Number(received.into()));
        if let Some(total) = total {
            details.insert("total_bytes".to_string(), Value::Number(total.into()));
        }
        self.logger.log_event(BuildLogEvent {
            level,
            status: BuildStatus::Running,
            op: Some("fetch".to_string()),
            message,
            object_hash: None,
            raw_log_path: None,
            details,
        });
    }

    // -- transport --------------------------------------------------------

    fn cached_token(&self) -> Option<String> {
        self.token.lock().expect("token poisoned").clone()
    }

    fn store_token(&self, token: &str) {
        *self.token.lock().expect("token poisoned") = Some(token.to_string());
    }

    async fn send(
        &self,
        url: &str,
        accept: &str,
        token: Option<String>,
    ) -> Result<reqwest::Response, RegistryError> {
        let mut request = self.client.get(url).header(reqwest::header::ACCEPT, accept);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.map_err(|error| {
            RegistryError::transient(
                format!("failed to reach '{url}': {}", error_with_causes(&error)),
                None,
            )
        })
    }

    /// One GET, authenticated. A 401 is protocol rather than failure: the
    /// registry is naming the token endpoint to go to, and the request is made
    /// again with what it hands back.
    async fn get(&self, url: &str, accept: &str) -> Result<reqwest::Response, RegistryError> {
        let response = self.send(url, accept, self.cached_token()).await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return check_status(response, url).await;
        }
        let challenge = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if challenge.is_empty() {
            // A 401 that names no way to authenticate is just a refusal.
            return check_status(response, url).await;
        }
        let token = self.fetch_token(&challenge).await?;
        self.store_token(&token);
        let response = self.send(url, accept, Some(token)).await?;
        check_status(response, url).await
    }

    async fn fetch_token(&self, challenge: &str) -> Result<String, RegistryError> {
        let (realm, service, scope) = parse_bearer_challenge(challenge).ok_or_else(|| {
            RegistryError::fatal(format!("failed to parse WWW-Authenticate: {challenge}"))
        })?;
        let mut request = self
            .client
            .get(&realm)
            .header(reqwest::header::ACCEPT, "application/json");
        if !service.is_empty() {
            request = request.query(&[("service", &service)]);
        }
        if !scope.is_empty() {
            request = request.query(&[("scope", &scope)]);
        }
        let response = request.send().await.map_err(|error| {
            RegistryError::transient(
                format!(
                    "token fetch from '{realm}' failed: {}",
                    error_with_causes(&error)
                ),
                None,
            )
        })?;
        let response = check_status(response, &realm).await?;
        let body = response.bytes().await.map_err(|error| {
            RegistryError::transient(
                format!(
                    "failed to read the token response from '{realm}': {}",
                    error_with_causes(&error)
                ),
                None,
            )
        })?;
        let body: Value = serde_json::from_slice(&body).map_err(|error| {
            RegistryError::fatal(format!("token response parse error: {error}"))
        })?;
        body["token"]
            .as_str()
            .or_else(|| body["access_token"].as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| RegistryError::fatal("token response missing 'token' field"))
    }

    /// Fetches a manifest whole -- they are small, and the bytes are both
    /// parsed and hashed -- returning it with the media type that says whether
    /// it is an index or an image.
    async fn get_manifest(&self, reference: &str) -> Result<(Vec<u8>, String), RegistryError> {
        let url = self.manifest_url(reference);
        let (bytes, media_type) = retrying(self.logger, self.policy, &url, || async {
            let response = self.get(&url, ACCEPT_MANIFESTS).await?;
            let media_type = content_type(&response);
            let bytes = response.bytes().await.map_err(|error| {
                RegistryError::transient(
                    format!(
                        "failed to read the manifest from '{url}': {}",
                        error_with_causes(&error)
                    ),
                    None,
                )
            })?;
            Ok((bytes.to_vec(), media_type))
        })
        .await?;
        self.received
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok((bytes, media_type))
    }

    /// Downloads one blob into the layout, verifying it as it goes.
    ///
    /// The digest is known before the first byte arrives, so hashing is done
    /// while writing and the content never exists in memory in full.
    async fn download_blob(
        &self,
        descriptor: &OciDescriptor,
        target_dir: &Path,
        label: &str,
    ) -> Result<(), RegistryError> {
        let url = self.blob_url(&descriptor.digest);
        let path = oci::blob_path(target_dir, &descriptor.digest);
        // Where this image's counter stood before the blob: an attempt that
        // fails part-way must not leave its bytes counted twice.
        let before = self.received.load(Ordering::Relaxed);
        retrying(self.logger, self.policy, &url, || async {
            self.received.store(before, Ordering::Relaxed);
            self.download_blob_once(&url, &descriptor.digest, &path, label)
                .await
        })
        .await
    }

    async fn download_blob_once(
        &self,
        url: &str,
        digest: &str,
        path: &Path,
        label: &str,
    ) -> Result<(), RegistryError> {
        let mut response = self.get(url, "application/octet-stream").await?;
        let mut file = tokio::fs::File::create(path).await.map_err(|error| {
            RegistryError::fatal(format!(
                "failed to create blob file '{}': {error}",
                path.display()
            ))
        })?;
        let mut hasher = Sha256::new();
        loop {
            let chunk = response.chunk().await.map_err(|error| {
                RegistryError::transient(
                    format!(
                        "failed to read the body of '{url}': {}",
                        error_with_causes(&error)
                    ),
                    None,
                )
            })?;
            let Some(bytes) = chunk else { break };
            hasher.update(&bytes);
            file.write_all(&bytes).await.map_err(|error| {
                RegistryError::fatal(format!(
                    "failed to write blob file '{}': {error}",
                    path.display()
                ))
            })?;
            self.received
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            self.tick(|| format!("pulling {label} from {}", self.host));
        }
        file.flush().await.map_err(|error| {
            RegistryError::fatal(format!(
                "failed to write blob file '{}': {error}",
                path.display()
            ))
        })?;
        let actual = format!("sha256:{:x}", hasher.finalize());
        if actual != digest {
            // The digest came out of a manifest that was itself verified, so a
            // mismatch here is not a stale pin -- it is content that arrived
            // damaged, which is exactly what another attempt is for.
            let _ = std::fs::remove_file(path);
            return Err(RegistryError::transient(
                format!("{label} arrived corrupted: expected {digest}, got {actual}"),
                None,
            ));
        }
        Ok(())
    }

    /// The digest the image's tag points at right now. Best effort by nature:
    /// it is only ever used to suggest a pin, so it takes one attempt and its
    /// own failure is not worth reporting.
    pub(super) async fn resolve_current_digest(&self) -> Result<String, RegistryError> {
        let url = self.manifest_url(&self.reference);
        let response = self.get(&url, ACCEPT_MANIFESTS).await?;
        let bytes = response.bytes().await.map_err(|error| {
            RegistryError::transient(
                format!(
                    "failed to read the manifest from '{url}': {}",
                    error_with_causes(&error)
                ),
                None,
            )
        })?;
        Ok(oci::sha256_digest(&bytes))
    }
}

/// Fetches the image `pinned_digest` names, for `platform`, into an OCI layout
/// at `target_dir`, and returns the digest of the manifest it stored.
pub(super) async fn pull_image(
    session: &Session<'_>,
    image: &str,
    pinned_digest: &str,
    platform: &OciPlatform,
    target_dir: &Path,
) -> Result<String, RegistryError> {
    session.milestone(format!("pulling OCI image {image}"));

    let (pinned_bytes, pinned_media_type) = session.get_manifest(pinned_digest).await?;
    let actual_digest = oci::sha256_digest(&pinned_bytes);
    if actual_digest != pinned_digest {
        return Err(RegistryError::fatal(format!(
            "manifest digest mismatch: expected {pinned_digest}, got {actual_digest}"
        )));
    }

    let manifest_bytes = if is_manifest_list(&pinned_media_type) {
        let platform_digest =
            select_platform_manifest(&pinned_bytes, &platform.os, &platform.architecture)?;
        session.milestone(format!(
            "selected {}/{} manifest {platform_digest}",
            platform.os, platform.architecture
        ));
        let (platform_bytes, _) = session.get_manifest(&platform_digest).await?;
        let actual = oci::sha256_digest(&platform_bytes);
        if actual != platform_digest {
            return Err(RegistryError::fatal(format!(
                "manifest digest mismatch: expected {platform_digest}, got {actual}"
            )));
        }
        platform_bytes
    } else {
        pinned_bytes
    };

    let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| RegistryError::fatal(format!("failed to parse manifest: {error}")))?;

    // The one download in the fetcher whose size is known before it starts:
    // every blob states its own, so the total is arithmetic rather than a
    // Content-Length the server may not send.
    let blob_bytes: u64 =
        manifest.config.size + manifest.layers.iter().map(|l| l.size).sum::<u64>();
    session.total.store(
        session.received.load(Ordering::Relaxed) + blob_bytes,
        Ordering::Relaxed,
    );

    oci::init_layout(target_dir)?;
    oci::write_blob(target_dir, &manifest_bytes, MEDIA_TYPE_OCI_MANIFEST)?;

    session.milestone(format!("fetching config blob {}", manifest.config.digest));
    session
        .download_blob(&manifest.config, target_dir, "config blob")
        .await?;

    let layers = manifest.layers.len();
    for (index, layer) in manifest.layers.iter().enumerate() {
        let label = format!("layer {}/{layers}", index + 1);
        session.milestone(format!("fetching {label} {}", layer.digest));
        session.download_blob(layer, target_dir, &label).await?;
    }

    let stored_manifest_digest = oci::sha256_digest(&manifest_bytes);
    oci::write_index(
        target_dir,
        OciDescriptor {
            media_type: MEDIA_TYPE_OCI_MANIFEST.to_string(),
            digest: stored_manifest_digest.clone(),
            size: manifest_bytes.len() as u64,
            platform: None,
            annotations: None,
        },
        None,
    )?;

    session.milestone(format!(
        "pulled {} bytes of {image}",
        session.received.load(Ordering::Relaxed)
    ));
    Ok(stored_manifest_digest)
}

/// Runs one request until it succeeds, is refused, or the budget runs out.
///
/// The same shape as the mirror walk's: transient failures wait and try again,
/// fatal ones return at once, and every wait is announced -- a retry that is
/// never logged is a slowdown nobody can account for afterwards.
async fn retrying<T, F, Fut>(
    logger: &Arc<dyn BuildLogger>,
    policy: HttpRetryPolicy,
    url: &str,
    mut attempt: F,
) -> Result<T, RegistryError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RegistryError>>,
{
    for number in 1..=policy.attempts {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Retry::After(retry_after) = error.retry else {
            return Err(error);
        };
        if number == policy.attempts {
            return Err(error);
        }
        let delay = policy.delay_before(number + 1, retry_after, url);
        let (message, details) =
            http::retry_notice(url, delay, number + 1, policy.attempts, &error.message);
        logger.log_event(BuildLogEvent {
            level: BuildLogLevel::Info,
            status: BuildStatus::Running,
            op: Some("fetch".to_string()),
            message,
            object_hash: None,
            raw_log_path: None,
            details,
        });
        tokio::time::sleep(delay).await;
    }
    unreachable!("the loop returns on the last attempt")
}

/// Turns a non-success status into an error, reading the same way the mirror
/// walk does: the server's own 5xx and a 429 are worth another attempt, every
/// other 4xx says the thing is not there.
async fn check_status(
    response: reqwest::Response,
    url: &str,
) -> Result<reqwest::Response, RegistryError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(http::parse_retry_after);
    let body = response.text().await.unwrap_or_default();
    let mut message = format!("'{url}': HTTP {status}");
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if !body.is_empty() {
        message.push_str(": ");
        if body.len() > ERROR_BODY_LIMIT {
            message.push_str(&body[..ERROR_BODY_LIMIT]);
            message.push('…');
        } else {
            message.push_str(&body);
        }
    }
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        Err(RegistryError::transient(message, retry_after))
    } else {
        Err(RegistryError::fatal(message))
    }
}

fn content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Plain HTTP for a registry on this machine, TLS for everything else: a local
/// test registry has no certificate, and a remote one has no excuse.
fn scheme_for(host: &str) -> &'static str {
    if host.starts_with("localhost") || host.starts_with("127.") {
        "http"
    } else {
        "https"
    }
}

/// Splits an image reference into `(registry_host, repository, reference)`.
///
/// Applies Docker Hub defaults: a bare name resolves to `registry-1.docker.io`
/// with a `library/` prefix, and a missing tag/digest defaults to `latest`. The
/// returned `reference` is either a tag or a `sha256:...` digest.
pub(super) fn parse_image_ref(image: &str) -> Result<(String, String, String), RegistryError> {
    if image.trim().is_empty() {
        return Err(RegistryError::fatal("image reference is empty"));
    }
    let (name_part, reference) = if let Some(pos) = image.rfind('@') {
        (&image[..pos], image[pos + 1..].to_string())
    } else if let Some(pos) = image.rfind(':') {
        let after = &image[pos + 1..];
        // A colon before a slash is a port, not a tag.
        if after.contains('/') {
            (image, "latest".to_string())
        } else {
            (&image[..pos], after.to_string())
        }
    } else {
        (image, "latest".to_string())
    };

    let (host, repository) = if let Some(slash) = name_part.find('/') {
        let first = &name_part[..slash];
        let rest = &name_part[slash + 1..];
        if first.contains('.') || first.contains(':') || first == "localhost" {
            (first.to_string(), rest.to_string())
        } else {
            ("docker.io".to_string(), name_part.to_string())
        }
    } else {
        ("docker.io".to_string(), format!("library/{name_part}"))
    };

    let registry_host = if host == "docker.io" {
        "registry-1.docker.io".to_string()
    } else {
        host
    };
    let repository = if registry_host == "registry-1.docker.io" && !repository.contains('/') {
        format!("library/{repository}")
    } else {
        repository
    };

    if repository.is_empty() {
        return Err(RegistryError::fatal(format!(
            "image reference '{image}' names no repository"
        )));
    }
    Ok((registry_host, repository, reference))
}

fn parse_bearer_challenge(header: &str) -> Option<(String, String, String)> {
    let header = header.strip_prefix("Bearer ")?;
    let realm = extract_quoted_value(header, "realm")?;
    let service = extract_quoted_value(header, "service").unwrap_or_default();
    let scope = extract_quoted_value(header, "scope").unwrap_or_default();
    Some((realm, service, scope))
}

fn extract_quoted_value(header: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=\"");
    let start = header.find(prefix.as_str())? + prefix.len();
    let end = start + header[start..].find('"')?;
    Some(header[start..end].to_string())
}

fn is_manifest_list(media_type: &str) -> bool {
    media_type == MEDIA_TYPE_DOCKER_MANIFEST_LIST || media_type == MEDIA_TYPE_OCI_INDEX
}

fn select_platform_manifest(
    index_bytes: &[u8],
    os: &str,
    arch: &str,
) -> Result<String, RegistryError> {
    let value: Value = serde_json::from_slice(index_bytes)
        .map_err(|error| RegistryError::fatal(format!("failed to parse manifest list: {error}")))?;
    let manifests = value["manifests"]
        .as_array()
        .ok_or_else(|| RegistryError::fatal("manifest list has no 'manifests' array"))?;
    for manifest in manifests {
        let manifest_os = manifest["platform"]["os"].as_str().unwrap_or_default();
        let manifest_arch = manifest["platform"]["architecture"]
            .as_str()
            .unwrap_or_default();
        if manifest_os == os && manifest_arch == arch {
            return manifest["digest"]
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| RegistryError::fatal("platform manifest has no digest"));
        }
    }
    Err(RegistryError::fatal(format!(
        "no {os}/{arch} manifest found in manifest list"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::NoopBuildLogger;
    use mockito::{Matcher, Server, ServerGuard};
    use std::sync::atomic::AtomicUsize;
    use tempfile::TempDir;

    fn logger() -> Arc<dyn BuildLogger> {
        Arc::new(NoopBuildLogger)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    fn layer_bytes() -> Vec<u8> {
        b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec()
    }

    fn config_bytes() -> Vec<u8> {
        br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},"config":{}}"#.to_vec()
    }

    fn descriptor(media_type: &str, data: &[u8]) -> OciDescriptor {
        OciDescriptor {
            media_type: media_type.to_string(),
            digest: oci::sha256_digest(data),
            size: data.len() as u64,
            platform: None,
            annotations: None,
        }
    }

    fn manifest_bytes(config: &[u8], layer: &[u8]) -> Vec<u8> {
        serde_json::to_vec(&OciManifest {
            schema_version: 2,
            config: descriptor("application/vnd.oci.image.config.v1+json", config),
            layers: vec![descriptor(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                layer,
            )],
        })
        .unwrap()
    }

    fn index_bytes(manifest: &[u8]) -> Vec<u8> {
        serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": oci::sha256_digest(manifest),
                "size": manifest.len(),
                "platform": { "os": "linux", "architecture": "amd64" }
            }]
        })
        .to_string()
        .into_bytes()
    }

    fn platform() -> OciPlatform {
        OciPlatform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
        }
    }

    /// `image` reference pointing a `Session` at a mockito server, which speaks
    /// plain HTTP on 127.0.0.1 -- the same address family the scheme rule
    /// treats as local.
    fn image_ref(server: &ServerGuard) -> String {
        let host = server.host_with_port();
        format!("{host}/team/app")
    }

    /// The whole registry conversation for one single-platform image, served
    /// without authentication.
    async fn serve_image(server: &mut ServerGuard, manifest: &[u8], config: &[u8], layer: &[u8]) {
        let manifest_digest = oci::sha256_digest(manifest);
        server
            .mock(
                "GET",
                format!("/v2/team/app/manifests/{manifest_digest}").as_str(),
            )
            .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
            .with_body(manifest)
            .create_async()
            .await;
        for blob in [config, layer] {
            server
                .mock(
                    "GET",
                    format!("/v2/team/app/blobs/{}", oci::sha256_digest(blob)).as_str(),
                )
                .with_body(blob)
                .create_async()
                .await;
        }
    }

    #[tokio::test]
    async fn pulls_an_image_into_a_layout() {
        let mut server = Server::new_async().await;
        let (config, layer) = (config_bytes(), layer_bytes());
        let manifest = manifest_bytes(&config, &layer);
        serve_image(&mut server, &manifest, &config, &layer).await;

        let target = TempDir::new().unwrap();
        let client = client();
        let logger = logger();
        let image = image_ref(&server);
        let session =
            Session::new(&client, &logger, HttpRetryPolicy::production(), &image).unwrap();
        let stored = pull_image(
            &session,
            &image,
            &oci::sha256_digest(&manifest),
            &platform(),
            target.path(),
        )
        .await
        .unwrap();

        assert_eq!(stored, oci::sha256_digest(&manifest));
        assert!(target.path().join("oci-layout").is_file());
        assert_eq!(
            std::fs::read(oci::blob_path(target.path(), &oci::sha256_digest(&layer))).unwrap(),
            layer
        );
        // Every byte of the image is counted, and the total is the exact sum
        // the manifest promised.
        let expected = (manifest.len() + config.len() + layer.len()) as u64;
        assert_eq!(session.received.load(Ordering::Relaxed), expected);
        assert_eq!(session.total.load(Ordering::Relaxed), expected);
    }

    #[tokio::test]
    async fn selects_the_platform_manifest_from_an_index() {
        let mut server = Server::new_async().await;
        let (config, layer) = (config_bytes(), layer_bytes());
        let manifest = manifest_bytes(&config, &layer);
        let index = index_bytes(&manifest);
        server
            .mock(
                "GET",
                format!("/v2/team/app/manifests/{}", oci::sha256_digest(&index)).as_str(),
            )
            .with_header("Content-Type", MEDIA_TYPE_OCI_INDEX)
            .with_body(index.clone())
            .create_async()
            .await;
        serve_image(&mut server, &manifest, &config, &layer).await;

        let target = TempDir::new().unwrap();
        let client = client();
        let logger = logger();
        let image = image_ref(&server);
        let session =
            Session::new(&client, &logger, HttpRetryPolicy::production(), &image).unwrap();
        let stored = pull_image(
            &session,
            &image,
            &oci::sha256_digest(&index),
            &platform(),
            target.path(),
        )
        .await
        .unwrap();

        // What is stored is the platform's manifest, not the index that led to
        // it: the layout describes one image.
        assert_eq!(stored, oci::sha256_digest(&manifest));
    }

    #[tokio::test]
    async fn one_bearer_token_serves_the_whole_pull() {
        let mut server = Server::new_async().await;
        let (config, layer) = (config_bytes(), layer_bytes());
        let manifest = manifest_bytes(&config, &layer);
        let manifest_digest = oci::sha256_digest(&manifest);
        let realm = format!("{}/token", server.url());

        let token_hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&token_hits);
        server
            .mock("GET", "/token")
            .match_query(Matcher::UrlEncoded(
                "scope".into(),
                "repository:team/app:pull".into(),
            ))
            .with_body_from_request(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
                br#"{"token":"sesame"}"#.to_vec()
            })
            .expect_at_least(1)
            .create_async()
            .await;

        let challenge = format!(
            r#"Bearer realm="{realm}",service="registry",scope="repository:team/app:pull""#
        );
        // Unauthenticated requests are refused with the challenge; the same
        // paths carrying the token succeed.
        for path in [
            format!("/v2/team/app/manifests/{manifest_digest}"),
            format!("/v2/team/app/blobs/{}", oci::sha256_digest(&config)),
            format!("/v2/team/app/blobs/{}", oci::sha256_digest(&layer)),
        ] {
            server
                .mock("GET", path.as_str())
                .match_header("authorization", Matcher::Missing)
                .with_status(401)
                .with_header("WWW-Authenticate", &challenge)
                .create_async()
                .await;
        }
        server
            .mock(
                "GET",
                format!("/v2/team/app/manifests/{manifest_digest}").as_str(),
            )
            .match_header("authorization", "Bearer sesame")
            .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
            .with_body(manifest.clone())
            .create_async()
            .await;
        for blob in [config.clone(), layer.clone()] {
            server
                .mock(
                    "GET",
                    format!("/v2/team/app/blobs/{}", oci::sha256_digest(&blob)).as_str(),
                )
                .match_header("authorization", "Bearer sesame")
                .with_body(blob)
                .create_async()
                .await;
        }

        let target = TempDir::new().unwrap();
        let client = client();
        let logger = logger();
        let image = image_ref(&server);
        let session =
            Session::new(&client, &logger, HttpRetryPolicy::production(), &image).unwrap();
        pull_image(
            &session,
            &image,
            &manifest_digest,
            &platform(),
            target.path(),
        )
        .await
        .unwrap();

        // Three requests needed authentication; the token was fetched for the
        // first and reused for the rest.
        assert_eq!(token_hits.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_stale_pin_fails_without_retrying() {
        let mut server = Server::new_async().await;
        let missing = oci::sha256_digest(b"not here");
        let hits = server
            .mock("GET", format!("/v2/team/app/manifests/{missing}").as_str())
            .with_status(404)
            .with_body(r#"{"errors":[{"code":"MANIFEST_UNKNOWN"}]}"#)
            .expect(1)
            .create_async()
            .await;

        let target = TempDir::new().unwrap();
        let client = client();
        let logger = logger();
        let image = image_ref(&server);
        let session =
            Session::new(&client, &logger, HttpRetryPolicy::production(), &image).unwrap();
        let error = pull_image(&session, &image, &missing, &platform(), target.path())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("404"), "{error}");
        assert!(matches!(error.retry, Retry::Never));
        // A 404 is the registry's answer, not a hiccup: asking again is waste.
        hits.assert_async().await;
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_then_succeeds() {
        let mut server = Server::new_async().await;
        let (config, layer) = (config_bytes(), layer_bytes());
        let manifest = manifest_bytes(&config, &layer);
        let manifest_digest = oci::sha256_digest(&manifest);
        let refused = server
            .mock(
                "GET",
                format!("/v2/team/app/manifests/{manifest_digest}").as_str(),
            )
            .with_status(503)
            .expect(1)
            .create_async()
            .await;
        server
            .mock(
                "GET",
                format!("/v2/team/app/manifests/{manifest_digest}").as_str(),
            )
            .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
            .with_body(manifest.clone())
            .create_async()
            .await;
        for blob in [config.clone(), layer.clone()] {
            server
                .mock(
                    "GET",
                    format!("/v2/team/app/blobs/{}", oci::sha256_digest(&blob)).as_str(),
                )
                .with_body(blob)
                .create_async()
                .await;
        }

        let target = TempDir::new().unwrap();
        let client = client();
        let logger = logger();
        let image = image_ref(&server);
        let session = Session::new(&client, &logger, HttpRetryPolicy::instant(), &image).unwrap();
        pull_image(
            &session,
            &image,
            &manifest_digest,
            &platform(),
            target.path(),
        )
        .await
        .unwrap();

        // The 503 was served, so the success came from a second attempt.
        refused.assert_async().await;
    }

    #[tokio::test]
    async fn a_corrupted_blob_is_retried_and_never_left_behind() {
        let mut server = Server::new_async().await;
        let (config, layer) = (config_bytes(), layer_bytes());
        let manifest = manifest_bytes(&config, &layer);
        let manifest_digest = oci::sha256_digest(&manifest);
        let layer_digest = oci::sha256_digest(&layer);
        server
            .mock(
                "GET",
                format!("/v2/team/app/manifests/{manifest_digest}").as_str(),
            )
            .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
            .with_body(manifest.clone())
            .create_async()
            .await;
        server
            .mock(
                "GET",
                format!("/v2/team/app/blobs/{}", oci::sha256_digest(&config)).as_str(),
            )
            .with_body(config.clone())
            .create_async()
            .await;
        // The layer arrives damaged once, then intact.
        let damaged = server
            .mock("GET", format!("/v2/team/app/blobs/{layer_digest}").as_str())
            .with_body(b"truncated")
            .expect(1)
            .create_async()
            .await;
        server
            .mock("GET", format!("/v2/team/app/blobs/{layer_digest}").as_str())
            .with_body(layer.clone())
            .create_async()
            .await;

        let target = TempDir::new().unwrap();
        let client = client();
        let logger = logger();
        let image = image_ref(&server);
        let session = Session::new(&client, &logger, HttpRetryPolicy::instant(), &image).unwrap();
        pull_image(
            &session,
            &image,
            &manifest_digest,
            &platform(),
            target.path(),
        )
        .await
        .unwrap();

        damaged.assert_async().await;
        // The retry replaced the damaged body rather than appending to it, and
        // the byte counter was rewound with it.
        assert_eq!(
            std::fs::read(oci::blob_path(target.path(), &layer_digest)).unwrap(),
            layer
        );
        assert_eq!(
            session.received.load(Ordering::Relaxed),
            (manifest.len() + config.len() + layer.len()) as u64
        );
    }

    #[tokio::test]
    async fn resolves_what_a_tag_points_at() {
        let mut server = Server::new_async().await;
        let (config, layer) = (config_bytes(), layer_bytes());
        let manifest = manifest_bytes(&config, &layer);
        server
            .mock("GET", "/v2/team/app/manifests/v3")
            .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
            .with_body(manifest.clone())
            .create_async()
            .await;

        let client = client();
        let logger = logger();
        let image = format!("{}:v3", image_ref(&server));
        let session =
            Session::new(&client, &logger, HttpRetryPolicy::production(), &image).unwrap();
        assert_eq!(
            session.resolve_current_digest().await.unwrap(),
            oci::sha256_digest(&manifest)
        );
    }

    #[test]
    fn image_references_split_the_way_registries_read_them() {
        assert_eq!(
            parse_image_ref("alpine").unwrap(),
            (
                "registry-1.docker.io".to_string(),
                "library/alpine".to_string(),
                "latest".to_string()
            )
        );
        assert_eq!(
            parse_image_ref("quay.io/team/app:1.2").unwrap(),
            (
                "quay.io".to_string(),
                "team/app".to_string(),
                "1.2".to_string()
            )
        );
        // A colon in the host is a port, and the tag is what follows the last
        // colon only when no slash comes after it.
        assert_eq!(
            parse_image_ref("localhost:5000/app").unwrap(),
            (
                "localhost:5000".to_string(),
                "app".to_string(),
                "latest".to_string()
            )
        );
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            parse_image_ref(&format!("quay.io/team/app@{digest}"))
                .unwrap()
                .2,
            digest
        );
    }

    #[test]
    fn a_bearer_challenge_yields_the_token_endpoint() {
        let (realm, service, scope) = parse_bearer_challenge(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:team/app:pull""#,
        )
        .unwrap();
        assert_eq!(realm, "https://auth.example/token");
        assert_eq!(service, "registry.example");
        assert_eq!(scope, "repository:team/app:pull");
        assert!(parse_bearer_challenge("Basic realm=\"x\"").is_none());
    }
}
