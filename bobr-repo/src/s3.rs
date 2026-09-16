//! Administrative S3 storage transport.

use crate::{
    FetchRequest, FetchResult, RepositoryError, RepositoryTlsConfig, RepositoryTransport,
    RepresentationMetadata,
};
use async_trait::async_trait;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::complete_multipart_upload::CompleteMultipartUploadError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketLocationConstraint, CompletedMultipartUpload, CompletedPart, CreateBucketConfiguration,
    Delete, ObjectIdentifier, PublicAccessBlockConfiguration,
};
use aws_smithy_http_client::{
    Builder as HttpClientBuilder,
    tls::{self, rustls_provider::CryptoMode},
};
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use aws_smithy_types::byte_stream::Length;
use aws_smithy_types::checksum_config::RequestChecksumCalculation;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use std::path::Path;
use url::Url;

const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024;
const MIN_MULTIPART_PART_SIZE: u64 = 16 * 1024 * 1024;
const MAX_MULTIPART_PARTS: u64 = 10_000;

/// Parsed S3 bucket and optional repository key prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Location {
    bucket: String,
    prefix: String,
}

/// One key returned by an S3 namespace scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredKey {
    /// Repository-relative key.
    pub key: String,
    /// Encoded bytes occupied by the S3 object.
    pub size: u64,
}

/// Result of a conditional immutable upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmutableUpload {
    /// This invocation created the key.
    Created,
    /// The key already existed and was left unchanged.
    AlreadyExists,
}

/// Changes made while initializing a dedicated repository bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketInitialization {
    /// Whether this invocation created the bucket.
    pub created: bool,
    /// Whether the S3 implementation accepted bucket-level Public Access Block.
    pub public_access_block_supported: bool,
    /// Whether this invocation installed or replaced the bucket policy.
    pub policy_updated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalPut {
    Written,
    PreconditionFailed,
    Conflict,
}

/// Result of fetching one mutable S3 object.
#[derive(Debug)]
pub struct S3Object {
    /// Exact response bytes.
    pub bytes: Vec<u8>,
    /// Opaque S3 entity tag.
    pub etag: String,
}

/// Administrative S3 repository backend using the standard AWS SDK chain.
#[derive(Debug, Clone)]
pub struct S3Repository {
    client: aws_sdk_s3::Client,
    location: S3Location,
}

/// S3-backed implementation of the repository reader transport.
#[derive(Debug, Clone)]
pub struct S3RepositoryTransport {
    repository: S3Repository,
    master_url: Url,
    data_base_url: Url,
}

impl S3Location {
    /// Parses `s3://bucket/optional-prefix`.
    pub fn parse(value: &str) -> Result<Self, RepositoryError> {
        let url = Url::parse(value)
            .map_err(|error| RepositoryError::new(format!("invalid S3 repository URI: {error}")))?;
        if url.scheme() != "s3"
            || url.username() != ""
            || url.password().is_some()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(RepositoryError::new(
                "repository must be an s3://bucket/optional-prefix URI",
            ));
        }
        let bucket = url
            .host_str()
            .filter(|bucket| !bucket.is_empty())
            .ok_or_else(|| RepositoryError::new("S3 repository URI has no bucket"))?
            .to_owned();
        let prefix = url.path().trim_matches('/').to_owned();
        if prefix
            .split('/')
            .any(|component| component == "." || component == "..")
        {
            return Err(RepositoryError::new(
                "S3 repository prefix is not canonical",
            ));
        }
        Ok(Self { bucket, prefix })
    }

    /// Requires this location to name a bucket rather than a key prefix.
    pub fn require_bucket_root(&self) -> Result<(), RepositoryError> {
        if self.prefix.is_empty() {
            Ok(())
        } else {
            Err(RepositoryError::new(
                "repository initialization requires a bucket root (s3://BUCKET)",
            ))
        }
    }

    fn key(&self, relative: &str) -> Result<String, RepositoryError> {
        if relative.is_empty()
            || relative.starts_with('/')
            || relative.ends_with('/')
            || relative
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(RepositoryError::new(format!(
                "invalid repository-relative S3 key '{relative}'"
            )));
        }
        Ok(if self.prefix.is_empty() {
            relative.to_owned()
        } else {
            format!("{}/{relative}", self.prefix)
        })
    }

    fn relative(&self, key: &str) -> Option<String> {
        if self.prefix.is_empty() {
            return Some(key.to_owned());
        }
        key.strip_prefix(&format!("{}/", self.prefix))
            .map(str::to_owned)
    }
}

impl S3Repository {
    /// Loads credentials, region, endpoint, and retry configuration from the
    /// standard AWS SDK configuration chain.
    pub async fn from_environment(
        location: S3Location,
        tls_config: &RepositoryTlsConfig,
    ) -> Result<Self, RepositoryError> {
        let http_client = HttpClientBuilder::new()
            .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
            .tls_context(tls_config.smithy_tls_context()?)
            .build_https();
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .http_client(http_client)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .load()
            .await;
        Ok(Self {
            client: aws_sdk_s3::Client::new(&config),
            location,
        })
    }

    /// Constructs a repository around an explicitly configured SDK client.
    pub fn new(client: aws_sdk_s3::Client, location: S3Location) -> Self {
        Self { client, location }
    }

    /// Creates or repairs a dedicated Bobr repository bucket.
    ///
    /// Initialization owns the complete bucket policy and therefore rejects
    /// repository locations with an S3 key prefix.
    pub async fn initialize_dedicated_bucket(
        &self,
    ) -> Result<BucketInitialization, RepositoryError> {
        self.location.require_bucket_root()?;

        let created = self.ensure_bucket().await?;
        let public_access_block_supported = self.configure_public_access_block().await?;
        let policy_updated = self.configure_public_read_policy().await?;
        Ok(BucketInitialization {
            created,
            public_access_block_supported,
            policy_updated,
        })
    }

    /// Fetches a small mutable object into memory.
    pub async fn get_bytes(
        &self,
        relative: &str,
        max_bytes: u64,
    ) -> Result<Option<S3Object>, RepositoryError> {
        let key = self.location.key(relative)?;
        let output = match self
            .client
            .get_object()
            .bucket(&self.location.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(output) => output,
            Err(error) if is_status(&error, 404, "NoSuchKey") => return Ok(None),
            Err(error) => return Err(s3_error("get object", relative, &error)),
        };
        if output.content_length().is_some_and(|length| {
            u64::try_from(length)
                .ok()
                .is_none_or(|length| length > max_bytes)
        }) {
            return Err(RepositoryError::new(format!(
                "S3 object '{relative}' exceeds its format size limit"
            )));
        }
        let etag = output
            .e_tag()
            .ok_or_else(|| RepositoryError::new(format!("S3 object '{relative}' has no ETag")))?
            .to_owned();
        let bytes = output
            .body
            .collect()
            .await
            .map_err(|error| RepositoryError::new(format!("failed to read '{relative}': {error}")))?
            .into_bytes();
        if bytes.len() as u64 > max_bytes {
            return Err(RepositoryError::new(format!(
                "S3 object '{relative}' exceeds its format size limit"
            )));
        }
        Ok(Some(S3Object {
            bytes: bytes.to_vec(),
            etag,
        }))
    }

    /// Conditionally creates one immutable repository key.
    pub async fn put_immutable(
        &self,
        relative: &str,
        source: &Path,
        content_type: &str,
        cache_control: &str,
    ) -> Result<ImmutableUpload, RepositoryError> {
        let size = std::fs::metadata(source)?.len();
        if size < MULTIPART_THRESHOLD {
            for _ in 0..4 {
                match self
                    .put_small(relative, source, content_type, cache_control, None, true)
                    .await?
                {
                    ConditionalPut::Written => return Ok(ImmutableUpload::Created),
                    ConditionalPut::PreconditionFailed => {
                        return Ok(ImmutableUpload::AlreadyExists);
                    }
                    ConditionalPut::Conflict => tokio::task::yield_now().await,
                }
            }
            return Err(RepositoryError::new(format!(
                "conditional write of '{relative}' repeatedly conflicted"
            )));
        }
        self.put_multipart_immutable(relative, source, content_type, cache_control)
            .await
    }

    /// Conditionally creates or replaces the mutable master.
    pub async fn put_master(
        &self,
        source: &Path,
        predecessor_etag: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let outcome = self
            .put_small(
                "master",
                source,
                "application/cose; cose-type=\"cose-sign1\"",
                "no-cache",
                predecessor_etag,
                predecessor_etag.is_none(),
            )
            .await?;
        match outcome {
            ConditionalPut::Written => Ok(()),
            ConditionalPut::PreconditionFailed | ConditionalPut::Conflict => Err(
                RepositoryError::new("conditional master publication failed; repository changed"),
            ),
        }
    }

    /// Lists all keys below the repository prefix.
    pub async fn list(&self) -> Result<Vec<StoredKey>, RepositoryError> {
        let mut continuation = None;
        let mut keys = Vec::new();
        loop {
            let output = self
                .client
                .list_objects_v2()
                .bucket(&self.location.bucket)
                .set_prefix(
                    (!self.location.prefix.is_empty())
                        .then(|| format!("{}/", self.location.prefix)),
                )
                .set_continuation_token(continuation)
                .send()
                .await
                .map_err(|error| s3_error("list repository", "", &error))?;
            for object in output.contents() {
                let Some(key) = object.key().and_then(|key| self.location.relative(key)) else {
                    continue;
                };
                let size = object
                    .size()
                    .and_then(|size| u64::try_from(size).ok())
                    .unwrap_or(0);
                keys.push(StoredKey { key, size });
            }
            if !output.is_truncated().unwrap_or(false) {
                break;
            }
            continuation = output.next_continuation_token().map(str::to_owned);
        }
        keys.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(keys)
    }

    /// Deletes repository-relative keys in S3-sized batches.
    pub async fn delete(&self, relative_keys: &[String]) -> Result<(), RepositoryError> {
        for chunk in relative_keys.chunks(1000) {
            let objects = chunk
                .iter()
                .map(|relative| {
                    ObjectIdentifier::builder()
                        .key(self.location.key(relative)?)
                        .build()
                        .map_err(|error| RepositoryError::new(error.to_string()))
                })
                .collect::<Result<Vec<_>, RepositoryError>>()?;
            let delete = Delete::builder()
                .set_objects(Some(objects))
                .quiet(true)
                .build()
                .map_err(|error| RepositoryError::new(error.to_string()))?;
            let output = self
                .client
                .delete_objects()
                .bucket(&self.location.bucket)
                .delete(delete)
                .send()
                .await
                .map_err(|error| s3_error("delete repository objects", "", &error))?;
            if !output.errors().is_empty() {
                return Err(RepositoryError::new(format!(
                    "S3 rejected deletion of {} repository object(s)",
                    output.errors().len()
                )));
            }
        }
        Ok(())
    }

    async fn ensure_bucket(&self) -> Result<bool, RepositoryError> {
        match self
            .client
            .head_bucket()
            .bucket(&self.location.bucket)
            .send()
            .await
        {
            Ok(_) => return Ok(false),
            Err(error) if is_status(&error, 404, "NotFound") => {}
            Err(error) => return Err(s3_error("inspect bucket", "", &error)),
        }

        let mut request = self.client.create_bucket().bucket(&self.location.bucket);
        if let Some(region) = self.client.config().region().map(AsRef::<str>::as_ref)
            && region != "us-east-1"
        {
            request = request.create_bucket_configuration(
                CreateBucketConfiguration::builder()
                    .location_constraint(BucketLocationConstraint::from(region))
                    .build(),
            );
        }
        match request.send().await {
            Ok(_) => Ok(true),
            Err(error) if service_code(&error) == Some("BucketAlreadyOwnedByYou") => Ok(false),
            Err(error) => Err(s3_error("create bucket", "", &error)),
        }
    }

    async fn configure_public_access_block(&self) -> Result<bool, RepositoryError> {
        let configuration = PublicAccessBlockConfiguration::builder()
            .block_public_acls(true)
            .ignore_public_acls(true)
            .block_public_policy(false)
            .restrict_public_buckets(false)
            .build();
        match self
            .client
            .put_public_access_block()
            .bucket(&self.location.bucket)
            .public_access_block_configuration(configuration)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if is_not_implemented(&error) => Ok(false),
            Err(error) => Err(s3_error("configure bucket Public Access Block", "", &error)),
        }
    }

    async fn configure_public_read_policy(&self) -> Result<bool, RepositoryError> {
        let expected = public_read_policy(&self.location.bucket);
        let current = match self
            .client
            .get_bucket_policy()
            .bucket(&self.location.bucket)
            .send()
            .await
        {
            Ok(output) => output
                .policy()
                .and_then(|policy| serde_json::from_str(policy).ok()),
            Err(error) if is_status(&error, 404, "NoSuchBucketPolicy") => None,
            Err(error) => return Err(s3_error("read bucket policy", "", &error)),
        };
        if current.as_ref() == Some(&expected) {
            return Ok(false);
        }
        let policy = serde_json::to_string(&expected).map_err(|error| {
            RepositoryError::new(format!("failed to encode bucket policy: {error}"))
        })?;
        self.client
            .put_bucket_policy()
            .bucket(&self.location.bucket)
            .policy(policy)
            .send()
            .await
            .map_err(|error| s3_error("configure bucket policy", "", &error))?;
        Ok(true)
    }

    async fn put_small(
        &self,
        relative: &str,
        source: &Path,
        content_type: &str,
        cache_control: &str,
        if_match: Option<&str>,
        if_absent: bool,
    ) -> Result<ConditionalPut, RepositoryError> {
        let key = self.location.key(relative)?;
        let body = ByteStream::from_path(source).await.map_err(|error| {
            RepositoryError::new(format!("failed to open upload body: {error}"))
        })?;
        let mut request = self
            .client
            .put_object()
            .bucket(&self.location.bucket)
            .key(key)
            .body(body)
            .content_type(content_type)
            .cache_control(cache_control);
        if let Some(etag) = if_match {
            request = request.if_match(etag);
        }
        if if_absent {
            request = request.if_none_match("*");
        }
        match request.send().await {
            Ok(_) => Ok(ConditionalPut::Written),
            Err(error) if is_status(&error, 412, "PreconditionFailed") => {
                Ok(ConditionalPut::PreconditionFailed)
            }
            Err(error) if is_status(&error, 409, "ConditionalRequestConflict") => {
                Ok(ConditionalPut::Conflict)
            }
            Err(error) => Err(s3_error("put object", relative, &error)),
        }
    }

    async fn put_multipart_immutable(
        &self,
        relative: &str,
        source: &Path,
        content_type: &str,
        cache_control: &str,
    ) -> Result<ImmutableUpload, RepositoryError> {
        let key = self.location.key(relative)?;
        let size = std::fs::metadata(source)?.len();
        for _ in 0..4 {
            let created = self
                .client
                .create_multipart_upload()
                .bucket(&self.location.bucket)
                .key(&key)
                .content_type(content_type)
                .cache_control(cache_control)
                .send()
                .await
                .map_err(|error| s3_error("create multipart upload", relative, &error))?;
            let upload_id = created
                .upload_id()
                .ok_or_else(|| RepositoryError::new("S3 returned no multipart upload id"))?
                .to_owned();
            let result = self
                .upload_parts(relative, source, &key, &upload_id, size)
                .await;
            if !matches!(result, Ok(ConditionalPut::Written)) {
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.location.bucket)
                    .key(&key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
            }
            match result? {
                ConditionalPut::Written => return Ok(ImmutableUpload::Created),
                ConditionalPut::PreconditionFailed => {
                    return Ok(ImmutableUpload::AlreadyExists);
                }
                ConditionalPut::Conflict => tokio::task::yield_now().await,
            }
        }
        Err(RepositoryError::new(format!(
            "conditional multipart write of '{relative}' repeatedly conflicted"
        )))
    }

    async fn upload_parts(
        &self,
        relative: &str,
        source: &Path,
        key: &str,
        upload_id: &str,
        size: u64,
    ) -> Result<ConditionalPut, RepositoryError> {
        let mut parts = Vec::new();
        let mut offset = 0;
        let mut number = 1;
        let part_size = multipart_part_size(size);
        while offset < size {
            let length = (size - offset).min(part_size);
            let body = ByteStream::read_from()
                .path(source)
                .offset(offset)
                .length(Length::Exact(length))
                .build()
                .await
                .map_err(|error| {
                    RepositoryError::new(format!("failed to read upload part: {error}"))
                })?;
            let output = self
                .client
                .upload_part()
                .bucket(&self.location.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(number)
                .body(body)
                .send()
                .await
                .map_err(|error| s3_error("upload multipart part", relative, &error))?;
            let etag = output
                .e_tag()
                .ok_or_else(|| RepositoryError::new("S3 upload part returned no ETag"))?;
            parts.push(
                CompletedPart::builder()
                    .part_number(number)
                    .e_tag(etag)
                    .build(),
            );
            offset += length;
            number += 1;
        }
        let upload = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        match self
            .client
            .complete_multipart_upload()
            .bucket(&self.location.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(upload)
            .if_none_match("*")
            .send()
            .await
        {
            Ok(_) => Ok(ConditionalPut::Written),
            Err(error) if complete_precondition_failed(&error) => {
                Ok(ConditionalPut::PreconditionFailed)
            }
            Err(error) if is_status(&error, 409, "ConditionalRequestConflict") => {
                Ok(ConditionalPut::Conflict)
            }
            Err(error) => Err(s3_error("complete multipart upload", relative, &error)),
        }
    }
}

fn multipart_part_size(size: u64) -> u64 {
    size.div_ceil(MAX_MULTIPART_PARTS)
        .max(MIN_MULTIPART_PART_SIZE)
}

impl S3RepositoryTransport {
    /// Maps one public master/data URL pair to an administrative S3 prefix.
    pub fn new(repository: S3Repository, master_url: Url, data_base_url: Url) -> Self {
        Self {
            repository,
            master_url,
            data_base_url,
        }
    }

    fn relative_key(&self, url: &Url) -> Result<String, RepositoryError> {
        if url == &self.master_url {
            return Ok("master".to_owned());
        }
        url.as_str()
            .strip_prefix(self.data_base_url.as_str())
            .filter(|relative| !relative.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                RepositoryError::new(format!("URL '{url}' is outside this S3 repository"))
            })
    }
}

#[async_trait]
impl RepositoryTransport for S3RepositoryTransport {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
        let relative = self.relative_key(&request.url)?;
        let key = self.repository.location.key(&relative)?;
        let mut get = self
            .repository
            .client
            .get_object()
            .bucket(&self.repository.location.bucket)
            .key(key);
        if let Some(etag) = request.if_none_match {
            get = get.if_none_match(etag);
        }
        let mut output = match get.send().await {
            Ok(output) => output,
            Err(error) if is_status(&error, 404, "NoSuchKey") => {
                return Ok(FetchResult::Missing);
            }
            Err(error) if is_status(&error, 304, "NotModified") => {
                return Ok(FetchResult::NotModified);
            }
            Err(error) => return Err(s3_error("fetch object", &relative, &error)),
        };
        if output.content_length().is_some_and(|length| {
            u64::try_from(length)
                .ok()
                .is_none_or(|length| length > request.max_bytes)
        }) {
            return Err(RepositoryError::new(
                "S3 response exceeds its format size limit",
            ));
        }
        let metadata = RepresentationMetadata {
            content_type: output.content_type().map(str::to_owned),
            cache_control: output.cache_control().map(str::to_owned),
            content_encoding: output.content_encoding().map(str::to_owned),
            etag: output.e_tag().map(str::to_owned),
        };
        let mut destination = tokio::fs::File::create(request.destination).await?;
        let mut received = 0u64;
        while let Some(chunk) = output.body.next().await {
            let chunk = chunk.map_err(|error| {
                RepositoryError::new(format!("failed to read S3 body: {error}"))
            })?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| RepositoryError::new("S3 response size overflow"))?;
            if received > request.max_bytes {
                return Err(RepositoryError::new(
                    "S3 response exceeds its format size limit",
                ));
            }
            tokio::io::AsyncWriteExt::write_all(&mut destination, &chunk).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut destination).await?;
        Ok(FetchResult::Stored(metadata))
    }
}

fn service_code<E, R>(error: &SdkError<E, R>) -> Option<&str>
where
    E: ProvideErrorMetadata,
{
    error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code)
}

fn is_status<E>(error: &SdkError<E, HttpResponse>, status: u16, code: &str) -> bool
where
    E: ProvideErrorMetadata,
{
    service_code(error) == Some(code)
        || error
            .raw_response()
            .is_some_and(|response| response.status().as_u16() == status)
}

fn is_not_implemented<E>(error: &SdkError<E, HttpResponse>) -> bool
where
    E: ProvideErrorMetadata,
{
    matches!(service_code(error), Some("NotImplemented" | "NotSupported"))
        || error
            .raw_response()
            .is_some_and(|response| response.status().as_u16() == 501)
}

fn public_read_policy(bucket: &str) -> serde_json::Value {
    let resource = |key: &str| format!("arn:aws:s3:::{bucket}/{key}");
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Sid": "BobrRepositoryPublicRead",
            "Effect": "Allow",
            "Principal": "*",
            "Action": "s3:GetObject",
            "Resource": [
                resource("master"),
                resource("b/*"),
                resource("r/*"),
                resource("lo/*"),
                resource("lf/*"),
                resource("o/*"),
                resource("f/*"),
            ],
        }],
    })
}

fn complete_precondition_failed(
    error: &SdkError<CompleteMultipartUploadError, HttpResponse>,
) -> bool {
    is_status(error, 412, "PreconditionFailed")
}

fn s3_error<E: ProvideErrorMetadata + std::fmt::Debug, R: std::fmt::Debug>(
    operation: &str,
    relative: &str,
    error: &SdkError<E, R>,
) -> RepositoryError {
    let suffix = if relative.is_empty() {
        String::new()
    } else {
        format!(" '{relative}'")
    };
    RepositoryError::new(format!("failed to {operation}{suffix}: {error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::put_object::PutObjectError;
    use aws_smithy_runtime_api::http::StatusCode;
    use aws_smithy_types::body::SdkBody;
    use aws_smithy_types::error::ErrorMetadata;

    #[test]
    fn parses_bucket_and_prefix() {
        let location = S3Location::parse("s3://example/a/b").unwrap();
        assert_eq!(location.bucket, "example");
        assert_eq!(location.prefix, "a/b");
        assert_eq!(location.key("o/hash").unwrap(), "a/b/o/hash");
        assert_eq!(location.relative("a/b/o/hash").as_deref(), Some("o/hash"));
    }

    #[test]
    fn rejects_non_s3_and_noncanonical_locations() {
        assert!(S3Location::parse("https://example/a").is_err());
        assert!(S3Location::parse("s3://example/a?query").is_err());
    }

    #[test]
    fn bucket_initialization_rejects_a_prefix() {
        assert!(
            S3Location::parse("s3://example/prefix")
                .unwrap()
                .require_bucket_root()
                .is_err()
        );
        S3Location::parse("s3://example")
            .unwrap()
            .require_bucket_root()
            .unwrap();
    }

    #[test]
    fn public_policy_exposes_only_repository_reads() {
        let policy = public_read_policy("example");
        let statement = &policy["Statement"][0];
        assert_eq!(statement["Action"], "s3:GetObject");
        assert_eq!(statement["Principal"], "*");
        assert_eq!(statement["Resource"].as_array().unwrap().len(), 7);
        assert!(
            statement["Resource"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("arn:aws:s3:::example/master"))
        );
        assert!(!policy.to_string().contains("ListBucket"));
    }

    #[test]
    fn status_matching_accepts_raw_http_status_and_service_code() {
        let raw = HttpResponse::new(StatusCode::try_from(412).unwrap(), SdkBody::empty());
        let raw_error = SdkError::service_error(
            PutObjectError::generic(ErrorMetadata::builder().build()),
            raw,
        );
        assert!(is_status(&raw_error, 412, "PreconditionFailed"));

        let raw = HttpResponse::new(StatusCode::try_from(500).unwrap(), SdkBody::empty());
        let coded_error = SdkError::service_error(
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build()),
            raw,
        );
        assert!(is_status(&coded_error, 412, "PreconditionFailed"));
        assert!(!is_status(&coded_error, 409, "ConditionalRequestConflict"));
    }

    #[test]
    fn multipart_part_size_respects_the_s3_part_count_limit() {
        let one_tib = 1024 * 1024 * 1024 * 1024;
        let size = multipart_part_size(one_tib);
        assert!(one_tib.div_ceil(size) <= MAX_MULTIPART_PARTS);
        assert_eq!(
            multipart_part_size(MULTIPART_THRESHOLD),
            MIN_MULTIPART_PART_SIZE
        );
    }
}
