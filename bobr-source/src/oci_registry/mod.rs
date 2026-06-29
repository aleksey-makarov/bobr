// This module is `pub` only under `test-support`; in the default build it is
// private, which makes its (intentionally public) API items "unreachable". That
// is by design, not a visibility mistake — silence the lint in that config.
#![cfg_attr(not(feature = "test-support"), allow(unreachable_pub))]

mod registry;

use crate::origin::{OriginContext, OriginHandler, OriginSpec, ParsedOrigin};
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
use std::path::PathBuf;

use registry::{fetch_image_authenticated_with_progress, resolve_current_digest};
// Exposed (with the module) only under `test-support`, for `bobr`'s tests.
#[cfg(feature = "test-support")]
pub use registry::fetch_image_authenticated;

const OCI_LAYOUT_SUBDIR: &str = "image";

static OCI_REGISTRY_ORIGIN_SPEC: OriginSpec = OriginSpec { tag: "OciRegistry" };

#[derive(Debug)]
enum OciRegistryOriginError {
    BuildFailed(String),
    FsFailed(String),
}

impl OciRegistryOriginError {
    fn message(&self) -> &str {
        match self {
            Self::BuildFailed(message) | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for OciRegistryOriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// [`OriginHandler`](crate::origin::OriginHandler) for `Oci` origins: parses an
/// image reference plus pinned digest and platform into a registry-backed
/// [`ParsedOrigin`](crate::origin::ParsedOrigin).
#[derive(Debug)]
pub struct OciRegistryOriginHandler;

#[derive(Debug, Clone)]
struct OciRegistryOrigin {
    image: String,
    digest: String,
    platform: OciPlatform,
}

/// OCI platform selected from an image index or manifest list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciPlatform {
    os: String,
    architecture: String,
}

impl OciPlatform {
    /// Creates a platform from its OCI `os` and `architecture` fields.
    pub fn new(os: String, architecture: String) -> Result<Self, String> {
        if os.trim().is_empty() {
            return Err("os must not be empty".to_string());
        }
        if architecture.trim().is_empty() {
            return Err("architecture must not be empty".to_string());
        }
        Ok(Self { os, architecture })
    }

    /// Returns the OCI platform operating system.
    pub fn os(&self) -> &str {
        &self.os
    }

    /// Returns the OCI platform architecture.
    pub fn architecture(&self) -> &str {
        &self.architecture
    }
}

impl OriginHandler for OciRegistryOriginHandler {
    fn spec(&self) -> &'static OriginSpec {
        &OCI_REGISTRY_ORIGIN_SPEC
    }

    fn parse(
        &self,
        mut object: Map<String, Value>,
        field_path: &str,
    ) -> Result<Box<dyn ParsedOrigin>, String> {
        let kind = take_string(&mut object, field_path, "tag")?;
        debug_assert_eq!(kind, "OciRegistry");
        let image = take_string(&mut object, field_path, "image")?;
        if image.trim().is_empty() {
            return Err(format!("{field_path}.image: image must not be empty"));
        }
        let digest = take_string(&mut object, field_path, "digest")?;
        if !is_valid_sha256_digest(&digest) {
            return Err(format!(
                "{field_path}.digest: invalid digest '{digest}'; expected format: sha256:<64 hex chars>"
            ));
        }
        let platform = take_platform(&mut object, field_path, "platform")?;
        if !object.is_empty() {
            return Err(format!(
                "{field_path}: unexpected fields: {}",
                object.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(Box::new(OciRegistryOrigin {
            image,
            digest,
            platform,
        }))
    }
}

impl ParsedOrigin for OciRegistryOrigin {
    fn spec(&self) -> &'static OriginSpec {
        &OCI_REGISTRY_ORIGIN_SPEC
    }

    fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String> {
        materialize_oci_registry_origin(cx, self).map_err(|error| error.to_string())
    }

    fn clone_box(&self) -> Box<dyn ParsedOrigin> {
        Box::new(self.clone())
    }
}

fn materialize_oci_registry_origin(
    cx: &OriginContext<'_>,
    origin: &OciRegistryOrigin,
) -> Result<PathBuf, OciRegistryOriginError> {
    let staged_path = cx.temp_root.join(OCI_LAYOUT_SUBDIR);
    fs::create_dir(&staged_path).map_err(|e| {
        OciRegistryOriginError::FsFailed(format!("failed to create staging dir: {e}"))
    })?;

    cx.milestone(format!("pulling OCI image {}", origin.image));
    // The registry's progress callback fires at manifest/blob boundaries
    // (already coarse), so surface each message as a progress tick.
    fetch_image_authenticated_with_progress(
        &origin.image,
        &origin.digest,
        &origin.platform,
        &staged_path,
        &mut |message: &str| cx.progress(message),
    )
    .map_err(|e| {
        let hint = resolve_current_digest(&origin.image)
            .map(|d| format!("\n    digest = \"{d}\","))
            .unwrap_or_default();
        OciRegistryOriginError::BuildFailed(format!("{e}{hint}"))
    })?;

    Ok(staged_path)
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

fn take_platform(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<OciPlatform, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    let Value::Object(mut platform) = value else {
        return Err(format!("{path}.{field}: expected object"));
    };
    let field_path = format!("{path}.{field}");
    let os = take_string(&mut platform, &field_path, "os")?;
    let architecture = take_string(&mut platform, &field_path, "architecture")?;
    if !platform.is_empty() {
        return Err(format!(
            "{field_path}: unexpected fields: {}",
            platform.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    OciPlatform::new(os, architecture).map_err(|message| format!("{field_path}.{message}"))
}

fn is_valid_sha256_digest(value: &str) -> bool {
    const PREFIX: &str = "sha256:";
    if !value.starts_with(PREFIX) {
        return false;
    }
    let hex = &value[PREFIX.len()..];
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::oci;
    use bobr_core::{CancellationToken, NoopBuildLogger};
    use mockito::Server;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    fn sample_digest() -> &'static str {
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }

    fn sample_platform_value() -> Value {
        serde_json::json!({
            "os": "linux",
            "architecture": "amd64"
        })
    }

    fn assert_no_ref_name_annotation(staged: &std::path::Path) {
        let index: Value =
            serde_json::from_slice(&fs::read(staged.join("index.json")).unwrap()).unwrap();
        let manifest = &index["manifests"][0];
        assert!(
            manifest.get("annotations").is_none(),
            "registry source index must not include image ref annotations: {index:#}"
        );
    }

    #[test]
    fn parse_valid_oci_registry_origin() {
        let origin = OciRegistryOriginHandler
            .parse(
                Map::from_iter([
                    ("tag".to_string(), Value::String("OciRegistry".to_string())),
                    (
                        "image".to_string(),
                        Value::String("docker.io/library/alpine:3.20".to_string()),
                    ),
                    (
                        "digest".to_string(),
                        Value::String(sample_digest().to_string()),
                    ),
                    ("platform".to_string(), sample_platform_value()),
                ]),
                "$.origin",
            )
            .unwrap();
        assert_eq!(origin.spec().tag, "OciRegistry");
    }

    #[test]
    fn reject_missing_platform() {
        let error = OciRegistryOriginHandler
            .parse(
                Map::from_iter([
                    ("tag".to_string(), Value::String("OciRegistry".to_string())),
                    (
                        "image".to_string(),
                        Value::String("docker.io/library/alpine:3.20".to_string()),
                    ),
                    (
                        "digest".to_string(),
                        Value::String(sample_digest().to_string()),
                    ),
                ]),
                "$.origin",
            )
            .unwrap_err();
        assert!(
            error.contains("missing required field 'platform'"),
            "{error}"
        );
    }

    #[test]
    fn reject_malformed_platform() {
        for (platform, expected) in [
            (
                serde_json::json!({"architecture": "amd64"}),
                "missing required field 'os'",
            ),
            (
                serde_json::json!({"os": "linux"}),
                "missing required field 'architecture'",
            ),
            (
                serde_json::json!({"os": "linux", "architecture": "amd64", "variant": "v8"}),
                "unexpected fields: variant",
            ),
            (
                serde_json::json!({"os": "", "architecture": "amd64"}),
                "os must not be empty",
            ),
            (
                serde_json::json!({"os": "linux", "architecture": ""}),
                "architecture must not be empty",
            ),
        ] {
            let error = OciRegistryOriginHandler
                .parse(
                    Map::from_iter([
                        ("tag".to_string(), Value::String("OciRegistry".to_string())),
                        (
                            "image".to_string(),
                            Value::String("docker.io/library/alpine:3.20".to_string()),
                        ),
                        (
                            "digest".to_string(),
                            Value::String(sample_digest().to_string()),
                        ),
                        ("platform".to_string(), platform),
                    ]),
                    "$.origin",
                )
                .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn reject_invalid_digest() {
        let error = OciRegistryOriginHandler
            .parse(
                Map::from_iter([
                    ("tag".to_string(), Value::String("OciRegistry".to_string())),
                    (
                        "image".to_string(),
                        Value::String("docker.io/library/alpine:3.20".to_string()),
                    ),
                    (
                        "digest".to_string(),
                        Value::String("sha256:short".to_string()),
                    ),
                ]),
                "$.origin",
            )
            .unwrap_err();
        assert!(error.contains("invalid digest"), "{error}");
    }

    #[test]
    fn reject_empty_image() {
        let error = OciRegistryOriginHandler
            .parse(
                Map::from_iter([
                    ("tag".to_string(), Value::String("OciRegistry".to_string())),
                    ("image".to_string(), Value::String("   ".to_string())),
                    (
                        "digest".to_string(),
                        Value::String(sample_digest().to_string()),
                    ),
                ]),
                "$.origin",
            )
            .unwrap_err();
        assert!(error.contains("image must not be empty"), "{error}");
    }

    #[test]
    fn materialize_writes_oci_layout() {
        let mut server = Server::new();

        let config_bytes = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":["sha256:0000000000000000000000000000000000000000000000000000000000000000"]},"config":{}}"#;
        let config_hex = format!("{:x}", Sha256::digest(config_bytes));
        let config_digest = format!("sha256:{config_hex}");

        let layer_bytes: &[u8] = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x00";
        let layer_hex = format!("{:x}", Sha256::digest(layer_bytes));
        let layer_digest = format!("sha256:{layer_hex}");

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {"mediaType": oci::MEDIA_TYPE_OCI_CONFIG, "digest": config_digest, "size": config_bytes.len()},
            "layers": [{"mediaType": oci::MEDIA_TYPE_OCI_LAYER, "digest": layer_digest, "size": layer_bytes.len()}]
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
            .with_header("Content-Type", oci::MEDIA_TYPE_OCI_MANIFEST)
            .with_body(manifest_bytes)
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

        let origin = OciRegistryOrigin {
            image: format!("{}/{repo}@{pinned_digest}", server.host_with_port()),
            digest: pinned_digest,
            platform: OciPlatform::new("linux".to_string(), "amd64".to_string()).unwrap(),
        };
        let temp = tempdir().unwrap();
        let logger = NoopBuildLogger;
        let cancellation = CancellationToken::new();
        let staged = materialize_oci_registry_origin(
            &OriginContext {
                temp_root: temp.path(),
                logger: &logger,
                cancellation: &cancellation,
            },
            &origin,
        )
        .unwrap();

        assert!(staged.join("oci-layout").exists());
        assert!(staged.join("index.json").exists());
        assert_no_ref_name_annotation(&staged);
        assert!(
            staged
                .join("blobs")
                .join("sha256")
                .join(&manifest_hex)
                .exists()
        );
        assert!(
            staged
                .join("blobs")
                .join("sha256")
                .join(&config_hex)
                .exists()
        );
        assert!(
            staged
                .join("blobs")
                .join("sha256")
                .join(&layer_hex)
                .exists()
        );
    }
}
