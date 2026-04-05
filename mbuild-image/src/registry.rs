use crate::oci::{self, MEDIA_TYPE_OCI_MANIFEST, OciDescriptor, OciManifest};
use std::fmt;
use std::io::Read;
use std::path::Path;

const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MEDIA_TYPE_DOCKER_MANIFEST_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json, ",
    "application/vnd.oci.image.manifest.v1+json, ",
    "application/vnd.docker.distribution.manifest.list.v2+json, ",
    "application/vnd.docker.distribution.manifest.v2+json"
);

#[derive(Debug)]
pub enum RegistryError {
    Http(String),
    Parse(String),
    Digest(String),
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

/// Parse an image reference into (registry_host, repository, reference).
///
/// Examples:
///   docker.io/library/ubuntu:20.04       → registry-1.docker.io, library/ubuntu, 20.04
///   ubuntu:20.04                          → registry-1.docker.io, library/ubuntu, 20.04
///   gcr.io/my-project/app:latest         → gcr.io, my-project/app, latest
///   localhost:5000/myimage@sha256:abc...  → localhost:5000, myimage, sha256:abc...
pub fn parse_image_ref(image: &str) -> Result<(String, String, String), RegistryError> {
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

/// Resolve the current manifest digest for an image reference (by tag).
/// Returns the digest of whatever the registry currently serves for that reference.
/// Useful for generating a `digest =` value to pin in a recipe.
pub fn resolve_current_digest(image: &str) -> Result<String, RegistryError> {
    let (registry_host, repository, reference) = parse_image_ref(image)?;
    let scheme = if registry_host.starts_with("localhost") || registry_host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    let url = format!("{scheme}://{registry_host}/v2/{repository}/manifests/{reference}");
    let (bytes, _) = get_with_bearer_auth(&url, ACCEPT_MANIFESTS)?;
    Ok(format!("sha256:{}", oci::sha256_hex(&bytes)))
}

/// Fetch a pinned image from an OCI/Docker registry and write it as an OCI
/// image layout directory to `target_dir` (which must already exist).
///
/// Handles Bearer auth challenges automatically.
pub fn fetch_image_authenticated(
    image: &str,
    pinned_digest: &str,
    target_dir: &Path,
) -> Result<String, RegistryError> {
    let (registry_host, repository, reference) = parse_image_ref(image)?;

    let scheme = if registry_host.starts_with("localhost") || registry_host.starts_with("127.") {
        "http"
    } else {
        "https"
    };

    // Always fetch by the pinned digest for determinism — the tag/reference in the
    // image string is only used to locate the registry and repository.
    let pinned_url =
        format!("{scheme}://{registry_host}/v2/{repository}/manifests/{pinned_digest}");
    let (pinned_bytes, pinned_media_type) = get_with_bearer_auth(&pinned_url, ACCEPT_MANIFESTS)?;

    // Verify that what the registry returned actually has the expected digest.
    let actual_digest = format!("sha256:{}", oci::sha256_hex(&pinned_bytes));
    if actual_digest != pinned_digest {
        return Err(RegistryError::Digest(format!(
            "manifest digest mismatch: expected {pinned_digest}, got {actual_digest}"
        )));
    }

    // If the pinned object is a manifest list / OCI index, select linux/amd64.
    let manifest_bytes = if is_manifest_list(&pinned_media_type) {
        let platform_digest = select_platform_manifest(&pinned_bytes, "linux", "amd64")?;
        let platform_url =
            format!("{scheme}://{registry_host}/v2/{repository}/manifests/{platform_digest}");
        let (platform_bytes, _) = get_with_bearer_auth(&platform_url, ACCEPT_MANIFESTS)?;
        verify_digest(&platform_bytes, &platform_digest)?;
        platform_bytes
    } else {
        pinned_bytes
    };

    // Suppress unused-variable warning when reference is not used for fetching.
    let _ = reference;

    let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| RegistryError::Parse(format!("failed to parse manifest: {e}")))?;

    oci::init_layout(target_dir)?;

    oci::write_blob(target_dir, &manifest_bytes, MEDIA_TYPE_OCI_MANIFEST)?;

    let config_bytes =
        get_blob_with_auth(scheme, &registry_host, &repository, &manifest.config.digest)?;
    verify_digest(&config_bytes, &manifest.config.digest)?;
    oci::write_blob(target_dir, &config_bytes, &manifest.config.media_type)?;

    for layer in &manifest.layers {
        let layer_bytes = get_blob_with_auth(scheme, &registry_host, &repository, &layer.digest)?;
        verify_digest(&layer_bytes, &layer.digest)?;
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
    oci::write_index(target_dir, manifest_descriptor, Some(image))?;

    Ok(stored_manifest_digest)
}

/// GET a URL, attempting unauthenticated first; if challenged with 401,
/// parse WWW-Authenticate Bearer and retry with token.
fn get_with_bearer_auth(url: &str, accept: &str) -> Result<(Vec<u8>, String), RegistryError> {
    let request = ureq::get(url).set("Accept", accept);
    match request.call() {
        Ok(resp) => read_response(resp),
        Err(ureq::Error::Status(401, resp)) => {
            let www_auth = resp.header("WWW-Authenticate").unwrap_or("").to_string();
            let token = fetch_bearer_token(&www_auth)?;
            let auth_request = ureq::get(url)
                .set("Accept", accept)
                .set("Authorization", &format!("Bearer {token}"));
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

fn fetch_bearer_token(www_authenticate: &str) -> Result<String, RegistryError> {
    let (realm, service, scope) = parse_bearer_challenge(www_authenticate).ok_or_else(|| {
        RegistryError::Http(format!(
            "failed to parse WWW-Authenticate: {www_authenticate}"
        ))
    })?;

    let resp = ureq::get(&realm)
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
) -> Result<Vec<u8>, RegistryError> {
    let url = format!("{scheme}://{registry_host}/v2/{repository}/blobs/{digest}");
    let (bytes, _) = get_with_bearer_auth(&url, "application/octet-stream")?;
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
    fn parse_image_ref_gcr() {
        let (host, repo, reference) =
            parse_image_ref("gcr.io/google-containers/pause:3.1").unwrap();
        assert_eq!(host, "gcr.io");
        assert_eq!(repo, "google-containers/pause");
        assert_eq!(reference, "3.1");
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
        use mockito::Server;
        use sha2::{Digest, Sha256};

        let mut server = Server::new();

        let config_bytes = b"{}";
        let config_hex = format!("{:x}", Sha256::digest(config_bytes));
        let config_digest = format!("sha256:{config_hex}");

        let layer_bytes: &[u8] = &[1, 2, 3, 4];
        let layer_hex = format!("{:x}", Sha256::digest(layer_bytes));
        let layer_digest = format!("sha256:{layer_hex}");

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_bytes.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_bytes.len()
            }]
        });
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
            .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
            .with_body(manifest_bytes.clone())
            .create();
        let _m3 = server
            .mock("GET", path_config.as_str())
            .with_status(200)
            .with_body(config_bytes.as_ref())
            .create();
        let _m4 = server
            .mock("GET", path_layer.as_str())
            .with_status(200)
            .with_body(layer_bytes)
            .create();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("oci");
        std::fs::create_dir(&target).unwrap();

        let image = format!("{}/{repo}@{pinned_digest}", server.host_with_port());
        let stored_manifest_digest =
            fetch_image_authenticated(&image, &pinned_digest, &target).unwrap();

        assert!(target.join("oci-layout").exists());
        assert!(target.join("index.json").exists());
        assert_eq!(stored_manifest_digest, format!("sha256:{manifest_hex}"));
        assert!(
            target
                .join("blobs")
                .join("sha256")
                .join(&manifest_hex)
                .exists()
        );
        assert!(
            target
                .join("blobs")
                .join("sha256")
                .join(&config_hex)
                .exists()
        );
        assert!(
            target
                .join("blobs")
                .join("sha256")
                .join(&layer_hex)
                .exists()
        );
    }
}
