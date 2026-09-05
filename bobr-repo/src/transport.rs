//! Asynchronous byte transport for public repository objects.

use crate::RepositoryError;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use url::Url;

/// Conditional fetch request passed to a repository transport.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// Absolute public HTTPS URL.
    pub url: Url,
    /// Destination file replaced with the response body on success.
    pub destination: std::path::PathBuf,
    /// Maximum accepted response body length.
    pub max_bytes: u64,
    /// Previously observed opaque ETag, if any.
    pub if_none_match: Option<String>,
}

/// Headers needed to validate a repository HTTP representation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepresentationMetadata {
    /// Response media type as sent by the origin.
    pub content_type: Option<String>,
    /// Response cache policy as sent by the origin.
    pub cache_control: Option<String>,
    /// HTTP content coding, which immutable repository responses forbid.
    pub content_encoding: Option<String>,
    /// Opaque HTTP entity tag usable for revalidation.
    pub etag: Option<String>,
}

/// Result of one conditional transport fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchResult {
    /// A new complete response body was stored at the requested destination.
    Stored(RepresentationMetadata),
    /// The cached response remains authoritative after HTTP revalidation.
    NotModified,
    /// The public repository has no such key.
    Missing,
}

/// Fetches public repository bytes without interpreting their format.
#[async_trait]
pub trait RepositoryTransport: Send + Sync {
    /// Fetches one URL and stores a successful body without buffering it whole.
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError>;
}

/// Anonymous HTTPS transport backed by `reqwest`.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    /// Creates an anonymous transport with conservative default timeouts.
    pub fn anonymous() -> Result<Self, RepositoryError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("bobr-repo/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|error| {
                RepositoryError::new(format!("failed to create HTTP client: {error}"))
            })?;
        Ok(Self { client })
    }

    /// Creates a transport using the supplied policy-configured HTTP client.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RepositoryTransport for HttpTransport {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
        if request.url.scheme() != "https" {
            return Err(RepositoryError::new("repository transport requires HTTPS"));
        }
        let mut builder = self.client.get(request.url.clone());
        if let Some(etag) = &request.if_none_match {
            builder = builder.header(IF_NONE_MATCH, etag);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| RepositoryError::new(format!("HTTP request failed: {error}")))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(FetchResult::NotModified);
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(FetchResult::Missing);
        }
        if !response.status().is_success() {
            return Err(RepositoryError::new(format!(
                "repository HTTP request returned {}",
                response.status()
            )));
        }
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > request.max_bytes)
        {
            return Err(RepositoryError::new(
                "repository response exceeds the format size limit",
            ));
        }
        let metadata = RepresentationMetadata {
            content_type: header_string(response.headers(), CONTENT_TYPE),
            cache_control: header_string(response.headers(), CACHE_CONTROL),
            content_encoding: header_string(response.headers(), CONTENT_ENCODING),
            etag: header_string(response.headers(), ETAG),
        };
        let mut file = tokio::fs::File::create(&request.destination).await?;
        let mut received = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                RepositoryError::new(format!("failed to read HTTP response body: {error}"))
            })?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| RepositoryError::new("repository response size overflow"))?;
            if received > request.max_bytes {
                return Err(RepositoryError::new(
                    "repository response exceeds the format size limit",
                ));
            }
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        Ok(FetchResult::Stored(metadata))
    }
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// In-memory transport used by deterministic integration tests and embedders.
#[derive(Debug, Clone, Default)]
pub struct MemoryTransport {
    state: Arc<Mutex<MemoryState>>,
}

#[derive(Debug, Default)]
struct MemoryState {
    objects: BTreeMap<String, MemoryObject>,
    requests: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
struct MemoryObject {
    body: Vec<u8>,
    metadata: RepresentationMetadata,
}

impl MemoryTransport {
    /// Inserts or replaces one test representation.
    pub fn insert(&self, url: Url, body: Vec<u8>, content_type: &str, cache_control: &str) {
        let etag = format!("\"{}\"", crate::MetadataHash::<()>::digest(&body));
        self.state
            .lock()
            .expect("memory transport lock")
            .objects
            .insert(
                url.into(),
                MemoryObject {
                    body,
                    metadata: RepresentationMetadata {
                        content_type: Some(content_type.to_owned()),
                        cache_control: Some(cache_control.to_owned()),
                        content_encoding: None,
                        etag: Some(etag),
                    },
                },
            );
    }

    /// Returns how many fetches addressed one URL.
    pub fn request_count(&self, url: &Url) -> usize {
        self.state
            .lock()
            .expect("memory transport lock")
            .requests
            .get(url.as_str())
            .copied()
            .unwrap_or(0)
    }
}

#[async_trait]
impl RepositoryTransport for MemoryTransport {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
        let object = {
            let mut state = self.state.lock().expect("memory transport lock");
            *state.requests.entry(request.url.to_string()).or_default() += 1;
            state.objects.get(request.url.as_str()).cloned()
        };
        let Some(object) = object else {
            return Ok(FetchResult::Missing);
        };
        if request.if_none_match == object.metadata.etag {
            return Ok(FetchResult::NotModified);
        }
        if object.body.len() as u64 > request.max_bytes {
            return Err(RepositoryError::new(
                "repository response exceeds the format size limit",
            ));
        }
        tokio::fs::write(request.destination, object.body).await?;
        Ok(FetchResult::Stored(object.metadata))
    }
}
