use super::OciPlatform;
use bobr_core::oci::{self, MEDIA_TYPE_OCI_MANIFEST, OciDescriptor, OciManifest};
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MEDIA_TYPE_DOCKER_MANIFEST_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json, ",
    "application/vnd.oci.image.manifest.v1+json, ",
    "application/vnd.docker.distribution.manifest.list.v2+json, ",
    "application/vnd.docker.distribution.manifest.v2+json"
);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Error from talking to an OCI registry: HTTP/auth, manifest or blob parsing,
/// digest verification, or local I/O while staging the image.
#[derive(Debug)]
pub enum RegistryError {
    /// HTTP transport failure or a non-success status from the registry.
    Http(String),
    /// Malformed manifest/index JSON or an unexpected media type.
    Parse(String),
    /// Content digest mismatch or an otherwise invalid digest.
    Digest(String),
    /// Local filesystem error while staging the image.
    Io(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Http(m)
            | RegistryError::Parse(m)
            | RegistryError::Digest(m)
            | RegistryError::Io(m) => f.write_str(m),
        }
    }
}

impl From<oci::OciError> for RegistryError {
    fn from(e: oci::OciError) -> Self {
        RegistryError::Io(e.to_string())
    }
}

impl From<ureq::Error> for RegistryError {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Status(status, resp) => {
                let body = resp.into_string().unwrap_or_default();
                RegistryError::Http(format!("HTTP {status}: {body}"))
            }
            ureq::Error::Transport(t) => RegistryError::Http(t.to_string()),
        }
    }
}

/// Splits an image reference into `(registry_host, repository, reference)`.
///
/// Applies Docker Hub defaults: a bare name resolves to `registry-1.docker.io`
/// with a `library/` prefix, and a missing tag/digest defaults to `latest`. The
/// returned `reference` is either a tag or a `sha256:...` digest.
pub(super) fn parse_image_ref(image: &str) -> Result<(String, String, String), RegistryError> {
    let (name_part, reference) = if let Some(pos) = image.rfind('@') {
        (&image[..pos], image[pos + 1..].to_string())
    } else if let Some(pos) = image.rfind(':') {
        let after = &image[pos + 1..];
        if after.contains('/') {
            (image, "latest".to_string())
        } else {
            (&image[..pos], after.to_string())
        }
    } else {
        (image, "latest".to_string())
    };

    let (host, repository) = if let Some(slash_pos) = name_part.find('/') {
        let first = &name_part[..slash_pos];
        let rest = &name_part[slash_pos + 1..];
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

    Ok((registry_host, repository, reference))
}

/// Resolves an image reference to the digest its tag currently points at, by
/// fetching the manifest and hashing it. Used to suggest a `digest = "..."` pin
/// when a pinned fetch fails.
pub(super) fn resolve_current_digest(image: &str) -> Result<String, RegistryError> {
    let (registry_host, repository, reference) = parse_image_ref(image)?;
    let scheme = if registry_host.starts_with("localhost") || registry_host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    let url = format!("{scheme}://{registry_host}/v2/{repository}/manifests/{reference}");
    let mut progress = |_message: &str| {};
    let (bytes, _) = get_with_bearer_auth(&url, ACCEPT_MANIFESTS, &mut progress)?;
    Ok(format!("sha256:{}", oci::sha256_hex(&bytes)))
}

/// Fetches the manifest and layers for `platform`, verifies the content matches
/// `pinned_digest`, and stages them under `target_dir` as an OCI layout,
/// returning the verified digest. Convenience wrapper over
/// [`fetch_image_authenticated_with_progress`] that discards progress.
///
/// Available in unit tests and under the `test-support` feature (where the
/// `oci_registry` module is public); not compiled into normal builds.
#[cfg(any(test, feature = "test-support"))]
pub fn fetch_image_authenticated(
    image: &str,
    pinned_digest: &str,
    platform: &OciPlatform,
    target_dir: &Path,
) -> Result<String, RegistryError> {
    let mut progress = |_message: &str| {};
    fetch_image_authenticated_with_progress(
        image,
        pinned_digest,
        platform,
        target_dir,
        &mut progress,
    )
}

/// Fetches the manifest and layers for `platform`, verifies them against
/// `pinned_digest`, and stages them under `target_dir` as an OCI layout,
/// returning the verified digest. Reports coarse progress: `progress` is called
/// with a short message at each manifest/blob step.
pub(super) fn fetch_image_authenticated_with_progress(
    image: &str,
    pinned_digest: &str,
    platform: &OciPlatform,
    target_dir: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<String, RegistryError> {
    let (registry_host, repository, reference) = parse_image_ref(image)?;

    let scheme = if registry_host.starts_with("localhost") || registry_host.starts_with("127.") {
        "http"
    } else {
        "https"
    };

    let pinned_url =
        format!("{scheme}://{registry_host}/v2/{repository}/manifests/{pinned_digest}");
    let pinned_message =
        format!("fetching pinned manifest {pinned_digest} from {registry_host}/{repository}");
    progress(&pinned_message);
    let (pinned_bytes, pinned_media_type) =
        get_with_bearer_auth(&pinned_url, ACCEPT_MANIFESTS, progress)?;

    let actual_digest = format!("sha256:{}", oci::sha256_hex(&pinned_bytes));
    if actual_digest != pinned_digest {
        return Err(RegistryError::Digest(format!(
            "manifest digest mismatch: expected {pinned_digest}, got {actual_digest}"
        )));
    }

    let manifest_bytes = if is_manifest_list(&pinned_media_type) {
        let platform_digest =
            select_platform_manifest(&pinned_bytes, platform.os(), platform.architecture())?;
        let platform_url =
            format!("{scheme}://{registry_host}/v2/{repository}/manifests/{platform_digest}");
        let platform_message = format!(
            "selected {}/{} manifest {platform_digest}; fetching platform manifest",
            platform.os(),
            platform.architecture()
        );
        progress(&platform_message);
        let (platform_bytes, _) = get_with_bearer_auth(&platform_url, ACCEPT_MANIFESTS, progress)?;
        verify_digest(&platform_bytes, &platform_digest)?;
        platform_bytes
    } else {
        pinned_bytes
    };

    let _ = reference;

    let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| RegistryError::Parse(format!("failed to parse manifest: {e}")))?;

    progress("initializing OCI image layout");
    oci::init_layout(target_dir)?;

    progress("writing manifest blob");
    oci::write_blob(target_dir, &manifest_bytes, MEDIA_TYPE_OCI_MANIFEST)?;

    let config_message = format!("fetching config blob {}", manifest.config.digest);
    progress(&config_message);
    let config_bytes = get_blob_with_auth(
        scheme,
        &registry_host,
        &repository,
        &manifest.config.digest,
        progress,
    )?;
    verify_digest(&config_bytes, &manifest.config.digest)?;
    progress("writing config blob");
    oci::write_blob(target_dir, &config_bytes, &manifest.config.media_type)?;

    for (index, layer) in manifest.layers.iter().enumerate() {
        let layer_message = format!(
            "fetching layer {}/{} {}",
            index + 1,
            manifest.layers.len(),
            layer.digest
        );
        progress(&layer_message);
        let layer_bytes =
            get_blob_with_auth(scheme, &registry_host, &repository, &layer.digest, progress)?;
        verify_digest(&layer_bytes, &layer.digest)?;
        let write_message = format!("writing layer {}/{}", index + 1, manifest.layers.len());
        progress(&write_message);
        oci::write_blob(target_dir, &layer_bytes, &layer.media_type)?;
    }

    let stored_manifest_digest = format!("sha256:{}", oci::sha256_hex(&manifest_bytes));
    let manifest_descriptor = OciDescriptor {
        media_type: MEDIA_TYPE_OCI_MANIFEST.to_string(),
        digest: stored_manifest_digest.clone(),
        size: manifest_bytes.len() as u64,
        platform: None,
        annotations: None,
    };
    progress("writing OCI index");
    oci::write_index(target_dir, manifest_descriptor, None)?;

    Ok(stored_manifest_digest)
}

fn http_request(url: &str, accept: &str) -> ureq::Request {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(HTTP_CONNECT_TIMEOUT)
        .timeout_read(HTTP_READ_TIMEOUT)
        .timeout_write(HTTP_WRITE_TIMEOUT)
        .build();
    agent
        .get(url)
        .set("Accept", accept)
        .timeout(HTTP_REQUEST_TIMEOUT)
}

fn get_with_bearer_auth(
    url: &str,
    accept: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<(Vec<u8>, String), RegistryError> {
    let request = http_request(url, accept);
    match request.call() {
        Ok(resp) => read_response(resp),
        Err(ureq::Error::Status(401, resp)) => {
            let www_auth = resp.header("WWW-Authenticate").unwrap_or("").to_string();
            progress("registry requested bearer token");
            let token = fetch_bearer_token(&www_auth, progress)?;
            progress("retrying registry request with bearer token");
            let auth_request =
                http_request(url, accept).set("Authorization", &format!("Bearer {token}"));
            let resp = auth_request.call()?;
            read_response(resp)
        }
        Err(other) => Err(RegistryError::from(other)),
    }
}

fn read_response(resp: ureq::Response) -> Result<(Vec<u8>, String), RegistryError> {
    let content_type = resp
        .header("Content-Type")
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| RegistryError::Io(format!("failed to read response body: {e}")))?;
    Ok((bytes, content_type))
}

fn fetch_bearer_token(
    www_authenticate: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<String, RegistryError> {
    let (realm, service, scope) = parse_bearer_challenge(www_authenticate).ok_or_else(|| {
        RegistryError::Http(format!(
            "failed to parse WWW-Authenticate: {www_authenticate}"
        ))
    })?;

    let token_message = format!("fetching bearer token from {realm}");
    progress(&token_message);
    let resp = http_request(&realm, "application/json")
        .query("service", &service)
        .query("scope", &scope)
        .call()
        .map_err(|e| RegistryError::Http(format!("token fetch failed: {e}")))?;

    let body_str = resp
        .into_string()
        .map_err(|e| RegistryError::Io(format!("failed to read token response: {e}")))?;
    let body: serde_json::Value = serde_json::from_str(&body_str)
        .map_err(|e| RegistryError::Parse(format!("token response parse error: {e}")))?;

    body["token"]
        .as_str()
        .or_else(|| body["access_token"].as_str())
        .map(|s: &str| s.to_string())
        .ok_or_else(|| RegistryError::Parse("token response missing 'token' field".to_string()))
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
    let value: serde_json::Value = serde_json::from_slice(index_bytes)
        .map_err(|e| RegistryError::Parse(format!("failed to parse manifest list: {e}")))?;
    let manifests = value["manifests"].as_array().ok_or_else(|| {
        RegistryError::Parse("manifest list has no 'manifests' array".to_string())
    })?;

    for m in manifests {
        let m_os = m["platform"]["os"].as_str().unwrap_or("");
        let m_arch = m["platform"]["architecture"].as_str().unwrap_or("");
        if m_os == os && m_arch == arch {
            return m["digest"].as_str().map(|s| s.to_string()).ok_or_else(|| {
                RegistryError::Parse("platform manifest has no digest".to_string())
            });
        }
    }

    Err(RegistryError::Parse(format!(
        "no {os}/{arch} manifest found in manifest list"
    )))
}

fn get_blob_with_auth(
    scheme: &str,
    registry_host: &str,
    repository: &str,
    digest: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<Vec<u8>, RegistryError> {
    let url = format!("{scheme}://{registry_host}/v2/{repository}/blobs/{digest}");
    let (bytes, _) = get_with_bearer_auth(&url, "application/octet-stream", progress)?;
    Ok(bytes)
}

fn verify_digest(data: &[u8], expected: &str) -> Result<(), RegistryError> {
    let actual = format!("sha256:{}", oci::sha256_hex(data));
    if actual != expected {
        return Err(RegistryError::Digest(format!(
            "digest mismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
    use sha2::{Digest, Sha256};

    fn sample_config_bytes() -> &'static [u8] {
        br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":["sha256:0000000000000000000000000000000000000000000000000000000000000000"]},"config":{}}"#
    }

    fn sample_layer_bytes() -> &'static [u8] {
        b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00"
    }

    fn sample_digests() -> (String, String) {
        let config_hex = format!("{:x}", Sha256::digest(sample_config_bytes()));
        let layer_hex = format!("{:x}", Sha256::digest(sample_layer_bytes()));
        (
            format!("sha256:{config_hex}"),
            format!("sha256:{layer_hex}"),
        )
    }

    fn platform(os: &str, architecture: &str) -> OciPlatform {
        OciPlatform::new(os.to_string(), architecture.to_string()).unwrap()
    }

    fn sample_manifest(
        config_digest: &str,
        layer_digest: &str,
        layer_len: usize,
    ) -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": oci::MEDIA_TYPE_OCI_CONFIG,
                "digest": config_digest,
                "size": sample_config_bytes().len()
            },
            "layers": [{
                "mediaType": oci::MEDIA_TYPE_OCI_LAYER,
                "digest": layer_digest,
                "size": layer_len
            }]
        })
    }

    fn index_json(path: &Path) -> serde_json::Value {
        let bytes = std::fs::read(path.join("index.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn assert_no_ref_name_annotation(path: &Path) {
        let index = index_json(path);
        let manifest = &index["manifests"][0];
        assert!(
            manifest.get("annotations").is_none(),
            "registry source index must not include image ref annotations: {index:#}"
        );
    }

    #[test]
    fn parse_image_ref_docker_hub_short() {
        let (host, repo, reference) = parse_image_ref("ubuntu:20.04").unwrap();
        assert_eq!(host, "registry-1.docker.io");
        assert_eq!(repo, "library/ubuntu");
        assert_eq!(reference, "20.04");
    }

    #[test]
    fn parse_image_ref_docker_hub_full() {
        let (host, repo, reference) =
            parse_image_ref("docker.io/library/buildpack-deps:bookworm").unwrap();
        assert_eq!(host, "registry-1.docker.io");
        assert_eq!(repo, "library/buildpack-deps");
        assert_eq!(reference, "bookworm");
    }

    #[test]
    fn parse_image_ref_localhost_with_digest() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let image = format!("localhost:5000/myrepo/myimage@{digest}");
        let (host, repo, reference) = parse_image_ref(&image).unwrap();
        assert_eq!(host, "localhost:5000");
        assert_eq!(repo, "myrepo/myimage");
        assert_eq!(reference, digest);
    }

    #[test]
    fn fetch_image_from_mock_registry() {
        let mut server = Server::new();
        let (config_digest, layer_digest) = sample_digests();
        let manifest = sample_manifest(&config_digest, &layer_digest, sample_layer_bytes().len());
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = format!("{:x}", Sha256::digest(&manifest_bytes));
        let pinned_digest = format!("sha256:{manifest_hex}");

        let repo = "testuser/testimage";
        let path_manifests = format!("/v2/{repo}/manifests/{pinned_digest}");
        let path_config = format!("/v2/{repo}/blobs/{config_digest}");
        let path_layer = format!("/v2/{repo}/blobs/{layer_digest}");
        let _m1 = server.mock("GET", "/v2/").with_status(200).create();
        let _m2 = server
            .mock("GET", path_manifests.as_str())
            .with_status(200)
            .with_header("Content-Type", MEDIA_TYPE_OCI_MANIFEST)
            .with_body(manifest_bytes)
            .create();
        let _m3 = server
            .mock("GET", path_config.as_str())
            .with_status(200)
            .with_body(sample_config_bytes())
            .create();
        let _m4 = server
            .mock("GET", path_layer.as_str())
            .with_status(200)
            .with_body(sample_layer_bytes())
            .create();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("oci");
        std::fs::create_dir(&target).unwrap();

        let image = format!("{}/{repo}@{pinned_digest}", server.host_with_port());
        let stored_manifest_digest =
            fetch_image_authenticated(&image, &pinned_digest, &platform("linux", "amd64"), &target)
                .unwrap();

        assert!(target.join("oci-layout").exists());
        assert!(target.join("index.json").exists());
        assert_no_ref_name_annotation(&target);
        assert_eq!(stored_manifest_digest, format!("sha256:{manifest_hex}"));
    }

    #[test]
    fn fetch_image_index_is_independent_of_image_ref() {
        let mut server = Server::new();
        let (config_digest, layer_digest) = sample_digests();
        let manifest = sample_manifest(&config_digest, &layer_digest, sample_layer_bytes().len());
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = format!("{:x}", Sha256::digest(&manifest_bytes));
        let pinned_digest = format!("sha256:{manifest_hex}");

        let mut mocks = Vec::new();
        for repo in ["mirror-a/testimage", "mirror-b/testimage"] {
            let path_manifests = format!("/v2/{repo}/manifests/{pinned_digest}");
            let path_config = format!("/v2/{repo}/blobs/{config_digest}");
            let path_layer = format!("/v2/{repo}/blobs/{layer_digest}");
            mocks.push(
                server
                    .mock("GET", path_manifests.as_str())
                    .with_status(200)
                    .with_header("Content-Type", MEDIA_TYPE_OCI_MANIFEST)
                    .with_body(manifest_bytes.clone())
                    .create(),
            );
            mocks.push(
                server
                    .mock("GET", path_config.as_str())
                    .with_status(200)
                    .with_body(sample_config_bytes())
                    .create(),
            );
            mocks.push(
                server
                    .mock("GET", path_layer.as_str())
                    .with_status(200)
                    .with_body(sample_layer_bytes())
                    .create(),
            );
        }

        let temp = tempfile::tempdir().unwrap();
        let target_a = temp.path().join("oci-a");
        let target_b = temp.path().join("oci-b");
        std::fs::create_dir(&target_a).unwrap();
        std::fs::create_dir(&target_b).unwrap();

        let image_a = format!("{}/mirror-a/testimage:latest", server.host_with_port());
        let image_b = format!("{}/mirror-b/testimage:bookworm", server.host_with_port());
        fetch_image_authenticated(
            &image_a,
            &pinned_digest,
            &platform("linux", "amd64"),
            &target_a,
        )
        .unwrap();
        fetch_image_authenticated(
            &image_b,
            &pinned_digest,
            &platform("linux", "amd64"),
            &target_b,
        )
        .unwrap();

        assert_no_ref_name_annotation(&target_a);
        assert_no_ref_name_annotation(&target_b);
        assert_eq!(
            std::fs::read(target_a.join("index.json")).unwrap(),
            std::fs::read(target_b.join("index.json")).unwrap()
        );
    }

    #[test]
    fn fetch_image_selects_linux_amd64_from_index() {
        let mut server = Server::new();
        let (config_digest, layer_digest) = sample_digests();
        let manifest = sample_manifest(&config_digest, &layer_digest, sample_layer_bytes().len());
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = format!("{:x}", Sha256::digest(&manifest_bytes));
        let manifest_digest = format!("sha256:{manifest_hex}");

        let other_manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": oci::MEDIA_TYPE_OCI_CONFIG,
                "digest": config_digest,
                "size": sample_config_bytes().len()
            },
            "layers": []
        });
        let other_manifest_bytes = serde_json::to_vec(&other_manifest).unwrap();
        let other_manifest_hex = format!("{:x}", Sha256::digest(&other_manifest_bytes));
        let other_manifest_digest = format!("sha256:{other_manifest_hex}");

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [
                {
                    "mediaType": MEDIA_TYPE_OCI_MANIFEST,
                    "digest": other_manifest_digest,
                    "size": other_manifest_bytes.len(),
                    "platform": { "os": "linux", "architecture": "arm64" }
                },
                {
                    "mediaType": MEDIA_TYPE_OCI_MANIFEST,
                    "digest": manifest_digest,
                    "size": manifest_bytes.len(),
                    "platform": { "os": "linux", "architecture": "amd64" }
                }
            ]
        });
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let index_hex = format!("{:x}", Sha256::digest(&index_bytes));
        let pinned_digest = format!("sha256:{index_hex}");

        let repo = "testuser/testimage";
        let path_index = format!("/v2/{repo}/manifests/{pinned_digest}");
        let path_manifest = format!("/v2/{repo}/manifests/{manifest_digest}");
        let path_config = format!("/v2/{repo}/blobs/{config_digest}");
        let path_layer = format!("/v2/{repo}/blobs/{layer_digest}");
        let _m1 = server.mock("GET", "/v2/").with_status(200).create();
        let _m2 = server
            .mock("GET", path_index.as_str())
            .with_status(200)
            .with_header("Content-Type", MEDIA_TYPE_OCI_INDEX)
            .with_body(index_bytes)
            .create();
        let _m3 = server
            .mock("GET", path_manifest.as_str())
            .with_status(200)
            .with_header("Content-Type", MEDIA_TYPE_OCI_MANIFEST)
            .with_body(manifest_bytes)
            .create();
        let _m4 = server
            .mock("GET", path_config.as_str())
            .with_status(200)
            .with_body(sample_config_bytes())
            .create();
        let _m5 = server
            .mock("GET", path_layer.as_str())
            .with_status(200)
            .with_body(sample_layer_bytes())
            .create();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("oci");
        std::fs::create_dir(&target).unwrap();

        let image = format!("{}/{repo}:latest", server.host_with_port());
        let stored_manifest_digest =
            fetch_image_authenticated(&image, &pinned_digest, &platform("linux", "amd64"), &target)
                .unwrap();
        assert_eq!(stored_manifest_digest, manifest_digest);
    }

    #[test]
    fn fetch_image_selects_requested_platform_from_index() {
        let mut server = Server::new();
        let (config_digest, layer_digest) = sample_digests();
        let arm64_manifest =
            sample_manifest(&config_digest, &layer_digest, sample_layer_bytes().len());
        let arm64_manifest_bytes = serde_json::to_vec(&arm64_manifest).unwrap();
        let arm64_manifest_hex = format!("{:x}", Sha256::digest(&arm64_manifest_bytes));
        let arm64_manifest_digest = format!("sha256:{arm64_manifest_hex}");

        let amd64_manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": oci::MEDIA_TYPE_OCI_CONFIG,
                "digest": config_digest,
                "size": sample_config_bytes().len()
            },
            "layers": []
        });
        let amd64_manifest_bytes = serde_json::to_vec(&amd64_manifest).unwrap();
        let amd64_manifest_hex = format!("{:x}", Sha256::digest(&amd64_manifest_bytes));
        let amd64_manifest_digest = format!("sha256:{amd64_manifest_hex}");

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [
                {
                    "mediaType": MEDIA_TYPE_OCI_MANIFEST,
                    "digest": arm64_manifest_digest,
                    "size": arm64_manifest_bytes.len(),
                    "platform": { "os": "linux", "architecture": "arm64" }
                },
                {
                    "mediaType": MEDIA_TYPE_OCI_MANIFEST,
                    "digest": amd64_manifest_digest,
                    "size": amd64_manifest_bytes.len(),
                    "platform": { "os": "linux", "architecture": "amd64" }
                }
            ]
        });
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let index_hex = format!("{:x}", Sha256::digest(&index_bytes));
        let pinned_digest = format!("sha256:{index_hex}");

        let repo = "testuser/testimage";
        let path_index = format!("/v2/{repo}/manifests/{pinned_digest}");
        let path_manifest = format!("/v2/{repo}/manifests/{arm64_manifest_digest}");
        let path_config = format!("/v2/{repo}/blobs/{config_digest}");
        let path_layer = format!("/v2/{repo}/blobs/{layer_digest}");
        let _m1 = server.mock("GET", "/v2/").with_status(200).create();
        let _m2 = server
            .mock("GET", path_index.as_str())
            .with_status(200)
            .with_header("Content-Type", MEDIA_TYPE_OCI_INDEX)
            .with_body(index_bytes)
            .create();
        let _m3 = server
            .mock("GET", path_manifest.as_str())
            .with_status(200)
            .with_header("Content-Type", MEDIA_TYPE_OCI_MANIFEST)
            .with_body(arm64_manifest_bytes)
            .create();
        let _m4 = server
            .mock("GET", path_config.as_str())
            .with_status(200)
            .with_body(sample_config_bytes())
            .create();
        let _m5 = server
            .mock("GET", path_layer.as_str())
            .with_status(200)
            .with_body(sample_layer_bytes())
            .create();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("oci");
        std::fs::create_dir(&target).unwrap();

        let image = format!("{}/{repo}:latest", server.host_with_port());
        let stored_manifest_digest =
            fetch_image_authenticated(&image, &pinned_digest, &platform("linux", "arm64"), &target)
                .unwrap();
        assert_eq!(stored_manifest_digest, arm64_manifest_digest);
    }

    #[test]
    fn fetch_image_fails_when_requested_platform_is_missing() {
        let mut server = Server::new();
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": MEDIA_TYPE_OCI_MANIFEST,
                "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 1,
                "platform": { "os": "linux", "architecture": "arm64" }
            }]
        });
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let index_hex = format!("{:x}", Sha256::digest(&index_bytes));
        let pinned_digest = format!("sha256:{index_hex}");
        let repo = "testuser/testimage";
        let path_index = format!("/v2/{repo}/manifests/{pinned_digest}");
        let _m1 = server.mock("GET", "/v2/").with_status(200).create();
        let _m2 = server
            .mock("GET", path_index.as_str())
            .with_status(200)
            .with_header("Content-Type", MEDIA_TYPE_OCI_INDEX)
            .with_body(index_bytes)
            .create();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("oci");
        std::fs::create_dir(&target).unwrap();

        let image = format!("{}/{repo}:latest", server.host_with_port());
        let error =
            fetch_image_authenticated(&image, &pinned_digest, &platform("linux", "amd64"), &target)
                .unwrap_err();
        assert!(
            error.to_string().contains("no linux/amd64 manifest found"),
            "{error}"
        );
    }

    #[test]
    fn fetch_image_handles_bearer_auth() {
        let mut registry = Server::new();
        let mut auth = Server::new();

        let (config_digest, layer_digest) = sample_digests();
        let manifest = sample_manifest(&config_digest, &layer_digest, sample_layer_bytes().len());
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = format!("{:x}", Sha256::digest(&manifest_bytes));
        let pinned_digest = format!("sha256:{manifest_hex}");

        let repo = "testuser/testimage";
        let manifest_path = format!("/v2/{repo}/manifests/{pinned_digest}");
        let config_path = format!("/v2/{repo}/blobs/{config_digest}");
        let layer_path = format!("/v2/{repo}/blobs/{layer_digest}");
        let auth_header = format!(
            "Bearer realm=\"{}/token\",service=\"registry.example\",scope=\"repository:{repo}:pull\"",
            auth.url()
        );
        let _token = auth
            .mock("GET", "/token")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("service".into(), "registry.example".into()),
                mockito::Matcher::UrlEncoded("scope".into(), format!("repository:{repo}:pull")),
            ]))
            .with_status(200)
            .with_body(r#"{"token":"secret-token"}"#)
            .create();
        let _manifest_unauth = registry
            .mock("GET", manifest_path.as_str())
            .with_status(401)
            .with_header("WWW-Authenticate", &auth_header)
            .create();
        let _manifest_auth = registry
            .mock("GET", manifest_path.as_str())
            .match_header("authorization", "Bearer secret-token")
            .with_status(200)
            .with_header("Content-Type", MEDIA_TYPE_OCI_MANIFEST)
            .with_body(manifest_bytes)
            .create();
        let _config_unauth = registry
            .mock("GET", config_path.as_str())
            .with_status(401)
            .with_header("WWW-Authenticate", &auth_header)
            .create();
        let _config = registry
            .mock("GET", config_path.as_str())
            .match_header("authorization", "Bearer secret-token")
            .with_status(200)
            .with_body(sample_config_bytes())
            .create();
        let _layer_unauth = registry
            .mock("GET", layer_path.as_str())
            .with_status(401)
            .with_header("WWW-Authenticate", &auth_header)
            .create();
        let _layer = registry
            .mock("GET", layer_path.as_str())
            .match_header("authorization", "Bearer secret-token")
            .with_status(200)
            .with_body(sample_layer_bytes())
            .create();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("oci");
        std::fs::create_dir(&target).unwrap();

        let image = format!("{}/{repo}:latest", registry.host_with_port());
        fetch_image_authenticated(&image, &pinned_digest, &platform("linux", "amd64"), &target)
            .unwrap();
        assert!(target.join("index.json").exists());
        assert_no_ref_name_annotation(&target);
    }
}
