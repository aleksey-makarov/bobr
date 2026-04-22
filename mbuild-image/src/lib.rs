mod layer;
mod oci;
mod registry;

use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    StagedBuildResult, TypedBuilder,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::fmt;
use std::fs;
use std::path::Path;

const OCI_LAYOUT_SUBDIR: &str = "image";

#[derive(Debug)]
enum ContainerImageError {
    InvalidConfig(String),
    BuildFailed(String),
    FsFailed(String),
}

impl ContainerImageError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(m) | Self::BuildFailed(m) | Self::FsFailed(m) => m,
        }
    }
}

impl fmt::Display for ContainerImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

#[derive(Debug)]
enum ImageError {
    InvalidConfig(String),
    InputResolutionFailed(String),
    BuildFailed(String),
    FsFailed(String),
}

impl ImageError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(m)
            | Self::InputResolutionFailed(m)
            | Self::BuildFailed(m)
            | Self::FsFailed(m) => m,
        }
    }
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type IResult<T> = Result<T, ImageError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerImageConfig {
    image: String,
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    ref_name: Option<String>,
}

pub struct ContainerImageBuilder;
pub struct ImageBuilder;

static CONTAINER_IMAGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "ContainerImage",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

static IMAGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "Image",
    required_inputs: &[],
    optional_inputs: &["base"],
    allow_extra_inputs: true,
};

impl TypedBuilder for ContainerImageBuilder {
    type Config = ContainerImageConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &CONTAINER_IMAGE_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        if !inputs.is_empty() {
            return Err(BuilderError::ExecutionFailed(
                "ContainerImage builder does not accept input objects".to_string(),
            ));
        }

        if config.image.trim().is_empty() {
            return Err(map_container_image_error(
                ContainerImageError::InvalidConfig("image must not be empty".to_string()),
            ));
        }
        if !is_valid_sha256_digest(&config.digest) {
            return Err(map_container_image_error(
                ContainerImageError::InvalidConfig(format!(
                    "invalid digest '{}'; expected format: sha256:<64 hex chars>",
                    config.digest
                )),
            ));
        }

        let staged_path = cx.temp_dir.join(OCI_LAYOUT_SUBDIR);
        fs::create_dir(&staged_path)
            .map_err(|e| {
                ContainerImageError::FsFailed(format!("failed to create staging dir: {e}"))
            })
            .map_err(map_container_image_error)?;

        cx.log_event(
            BuildLogLevel::Info,
            "fetch",
            format!("fetching image '{}' from registry", config.image),
        );
        let mut progress = |message: &str| {
            cx.log_event(BuildLogLevel::Info, "fetch-progress", message.to_string());
        };
        let manifest_digest = registry::fetch_image_authenticated_with_progress(
            &config.image,
            &config.digest,
            &staged_path,
            &mut progress,
        )
        .map_err(|e| {
            // On digest mismatch, resolve the current digest by tag and show a helpful hint.
            let hint = registry::resolve_current_digest(&config.image)
                .map(|d| format!("\n    digest = \"{d}\","))
                .unwrap_or_default();
            ContainerImageError::BuildFailed(format!("{e}{hint}"))
        })
        .map_err(map_container_image_error)?;

        let mut meta = Map::new();
        meta.insert(
            "manifest_digest".to_string(),
            Value::String(manifest_digest),
        );

        Ok(StagedBuildResult { meta, staged_path })
    }
}

impl TypedBuilder for ImageBuilder {
    type Config = ImageConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &IMAGE_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        let base = inputs.optional("base");
        let binaries = inputs
            .extras(&IMAGE_SPEC)
            .map(|(_, object)| object.clone())
            .collect::<Vec<_>>();
        validate_image_config(&config, base, &binaries).map_err(map_image_error)?;

        let mode = effective_image_mode(&config, base).map_err(map_image_error)?;
        cx.log_event(
            BuildLogLevel::Info,
            "prepare",
            format!("building image in '{}' mode", mode),
        );

        let staged_path = cx.temp_dir.join(OCI_LAYOUT_SUBDIR);
        fs::create_dir(&staged_path)
            .map_err(|e| ImageError::FsFailed(format!("failed to create staging dir: {e}")))
            .map_err(map_image_error)?;

        let ref_name = config.ref_name.as_deref();
        let manifest_digest = match mode {
            "bootstrap" => {
                run_bootstrap_mode(&staged_path, &binaries, ref_name).map_err(map_image_error)?
            }
            "layered" => {
                let base = base.unwrap();
                run_layered_mode(&staged_path, base, &binaries, ref_name)
                    .map_err(map_image_error)?
            }
            _ => unreachable!(),
        };

        let mut meta = Map::new();
        meta.insert(
            "manifest_digest".to_string(),
            Value::String(manifest_digest),
        );

        Ok(StagedBuildResult { meta, staged_path })
    }
}

fn validate_image_config(
    config: &ImageConfig,
    base: Option<&BuilderInputObject>,
    binaries: &[BuilderInputObject],
) -> IResult<()> {
    if binaries.is_empty() {
        return Err(ImageError::InvalidConfig(
            "Image builder requires at least one directory input".to_string(),
        ));
    }
    if let Some(mode) = &config.mode {
        if mode != "bootstrap" && mode != "layered" {
            return Err(ImageError::InvalidConfig(format!(
                "invalid image mode '{}'; expected 'bootstrap' or 'layered'",
                mode
            )));
        }
    }
    if let Some(base) = base {
        if !base.object_path.is_dir() {
            return Err(ImageError::InputResolutionFailed(format!(
                "base input must resolve to a directory: {}",
                base.object_path.display()
            )));
        }
        if !base.object_path.join("oci-layout").exists() {
            return Err(ImageError::InputResolutionFailed(format!(
                "base input is not a valid OCI layout directory: {}",
                base.object_path.display()
            )));
        }
    }
    for (index, binary) in binaries.iter().enumerate() {
        if !binary.object_path.is_dir() {
            return Err(ImageError::InputResolutionFailed(format!(
                "inputs[{index}] must resolve to a directory: {}",
                binary.object_path.display()
            )));
        }
    }
    if matches!(config.mode.as_deref(), Some("layered")) && base.is_none() {
        return Err(ImageError::InvalidConfig(
            "image mode 'layered' requires a base OCI image input".to_string(),
        ));
    }
    Ok(())
}

fn effective_image_mode(
    config: &ImageConfig,
    base: Option<&BuilderInputObject>,
) -> IResult<&'static str> {
    match (config.mode.as_deref(), base.is_some()) {
        (Some("bootstrap"), false) => Ok("bootstrap"),
        (Some("layered"), true) => Ok("layered"),
        (Some("bootstrap"), true) => Err(ImageError::InvalidConfig(
            "image mode 'bootstrap' is incompatible with a base OCI image input".to_string(),
        )),
        (Some("layered"), false) => Err(ImageError::InvalidConfig(
            "image mode 'layered' requires a base OCI image input".to_string(),
        )),
        (None, true) => Ok("layered"),
        (None, false) => Ok("bootstrap"),
        _ => unreachable!(),
    }
}

fn is_valid_sha256_digest(value: &str) -> bool {
    const PREFIX: &str = "sha256:";
    if !value.starts_with(PREFIX) {
        return false;
    }
    let hex = &value[PREFIX.len()..];
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Build a bootstrap OCI image from scratch (no base).
/// Returns the manifest digest.
fn run_bootstrap_mode(
    staging_dir: &Path,
    binaries: &[BuilderInputObject],
    ref_name: Option<&str>,
) -> IResult<String> {
    oci::init_layout(staging_dir).map_err(|e| ImageError::FsFailed(e.to_string()))?;

    let binary_paths: Vec<&Path> = binaries.iter().map(|b| b.object_path.as_path()).collect();
    let layer =
        layer::create_layer(&binary_paths).map_err(|e| ImageError::BuildFailed(e.to_string()))?;

    let layer_desc = oci::write_blob(staging_dir, &layer.compressed, oci::MEDIA_TYPE_OCI_LAYER)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    let config = json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": [layer.diff_id]
        },
        "config": {}
    });
    let config_bytes = serde_json::to_vec(&config)
        .map_err(|e| ImageError::FsFailed(format!("failed to serialize config: {e}")))?;
    let config_desc = oci::write_blob(staging_dir, &config_bytes, oci::MEDIA_TYPE_OCI_CONFIG)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    let manifest = oci::OciManifest {
        schema_version: 2,
        config: config_desc,
        layers: vec![layer_desc],
    };
    let manifest_bytes = serde_json::to_vec(&manifest)
        .map_err(|e| ImageError::FsFailed(format!("failed to serialize manifest: {e}")))?;
    let manifest_desc = oci::write_blob(staging_dir, &manifest_bytes, oci::MEDIA_TYPE_OCI_MANIFEST)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    let manifest_digest = manifest_desc.digest.clone();
    oci::write_index(staging_dir, manifest_desc, ref_name)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    Ok(manifest_digest)
}

/// Build a layered OCI image on top of a base.
/// Returns the manifest digest.
fn run_layered_mode(
    staging_dir: &Path,
    base: &BuilderInputObject,
    binaries: &[BuilderInputObject],
    ref_name: Option<&str>,
) -> IResult<String> {
    let base_dir = &base.object_path;

    // Read base manifest and config.
    let base_manifest = oci::read_oci_manifest(base_dir)
        .map_err(|e| ImageError::InputResolutionFailed(e.to_string()))?;
    let base_config = oci::read_config(base_dir, &base_manifest)
        .map_err(|e| ImageError::InputResolutionFailed(e.to_string()))?;

    // Initialize new layout and hardlink all base blobs.
    oci::init_layout(staging_dir).map_err(|e| ImageError::FsFailed(e.to_string()))?;
    oci::hardlink_layer_blobs(base_dir, staging_dir)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    // Build new layer from binary outputs.
    let binary_paths: Vec<&Path> = binaries.iter().map(|b| b.object_path.as_path()).collect();
    let layer =
        layer::create_layer(&binary_paths).map_err(|e| ImageError::BuildFailed(e.to_string()))?;
    let layer_desc = oci::write_blob(staging_dir, &layer.compressed, oci::MEDIA_TYPE_OCI_LAYER)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    // Synthesize new config: extend base diff_ids with the new layer's diffID.
    let mut new_config = base_config;
    if let Some(diff_ids) = new_config
        .get_mut("rootfs")
        .and_then(|r| r.get_mut("diff_ids"))
        .and_then(Value::as_array_mut)
    {
        diff_ids.push(Value::String(layer.diff_id.clone()));
    } else {
        new_config["rootfs"] = json!({
            "type": "layers",
            "diff_ids": [layer.diff_id]
        });
    }
    let config_bytes = serde_json::to_vec(&new_config)
        .map_err(|e| ImageError::FsFailed(format!("failed to serialize config: {e}")))?;
    let config_desc = oci::write_blob(staging_dir, &config_bytes, oci::MEDIA_TYPE_OCI_CONFIG)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    // Synthesize new manifest: extend base layers with the new layer.
    let mut new_layers = base_manifest.layers.clone();
    new_layers.push(layer_desc);
    let new_manifest = oci::OciManifest {
        schema_version: 2,
        config: config_desc,
        layers: new_layers,
    };
    let manifest_bytes = serde_json::to_vec(&new_manifest)
        .map_err(|e| ImageError::FsFailed(format!("failed to serialize manifest: {e}")))?;
    let manifest_desc = oci::write_blob(staging_dir, &manifest_bytes, oci::MEDIA_TYPE_OCI_MANIFEST)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    let manifest_digest = manifest_desc.digest.clone();
    oci::write_index(staging_dir, manifest_desc, ref_name)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    Ok(manifest_digest)
}

fn map_container_image_error(error: ContainerImageError) -> BuilderError {
    match error {
        ContainerImageError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        ContainerImageError::BuildFailed(message) | ContainerImageError::FsFailed(message) => {
            BuilderError::ExecutionFailed(message)
        }
    }
}

fn map_image_error(error: ImageError) -> BuilderError {
    match error {
        ImageError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        ImageError::InputResolutionFailed(message)
        | ImageError::BuildFailed(message)
        | ImageError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{Builder, BuilderInputObject, BuilderInputs};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn build_context(root: &Path) -> BuildContext {
        let state_dir = root.join("builder");
        let temp_dir = root.join("tmp");
        fs::create_dir_all(&state_dir).unwrap();
        mbuild_core::fsutil::recreate_empty_dir_force(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    /// Create a minimal valid OCI layout directory and return the path to it.
    fn create_test_oci_layout(dir: &Path, name: &str) -> PathBuf {
        let oci_dir = dir.join(name);
        fs::create_dir_all(oci_dir.join("blobs").join("sha256")).unwrap();
        fs::write(
            oci_dir.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let config_bytes = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":["sha256:0000000000000000000000000000000000000000000000000000000000000000"]},"config":{}}"#;
        let config_hex = format!("{:x}", Sha256::digest(config_bytes));
        let config_digest = format!("sha256:{config_hex}");
        fs::write(
            oci_dir.join("blobs").join("sha256").join(&config_hex),
            config_bytes,
        )
        .unwrap();

        // Minimal gzip of an empty tar (the magic bytes for gzip(empty tar)).
        let layer_bytes: &[u8] =
            b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let layer_hex = format!("{:x}", Sha256::digest(layer_bytes));
        let layer_digest = format!("sha256:{layer_hex}");
        fs::write(
            oci_dir.join("blobs").join("sha256").join(&layer_hex),
            layer_bytes,
        )
        .unwrap();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": oci::MEDIA_TYPE_OCI_CONFIG,
                "digest": config_digest,
                "size": config_bytes.len()
            },
            "layers": [{
                "mediaType": oci::MEDIA_TYPE_OCI_LAYER,
                "digest": layer_digest,
                "size": layer_bytes.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = format!("{:x}", Sha256::digest(&manifest_bytes));
        let manifest_digest = format!("sha256:{manifest_hex}");
        fs::write(
            oci_dir.join("blobs").join("sha256").join(&manifest_hex),
            &manifest_bytes,
        )
        .unwrap();

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": oci::MEDIA_TYPE_OCI_MANIFEST,
                "digest": manifest_digest,
                "size": manifest_bytes.len()
            }]
        });
        fs::write(
            oci_dir.join("index.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();

        oci_dir
    }

    fn resolved_binary_output(root: &Path, name: &str) -> BuilderInputObject {
        let object_path = root.join(name);
        fs::create_dir_all(&object_path).unwrap();
        fs::write(object_path.join("README.txt"), b"hello image\n").unwrap();
        BuilderInputObject {
            object_path,
            meta: Map::new(),
        }
    }

    fn resolved_base_image(root: &Path) -> BuilderInputObject {
        let oci_dir = create_test_oci_layout(root, "base-image");
        BuilderInputObject {
            object_path: oci_dir,
            meta: Map::new(),
        }
    }

    fn sample_digest() -> &'static str {
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }

    // -----------------------------------------------------------------------
    // ContainerImage builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn container_image_builder_fetches_and_writes_oci_layout() {
        use mockito::Server;

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

        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let image = format!("{}/{repo}@{pinned_digest}", server.host_with_port());

        let result = ContainerImageBuilder
            .build_typed(
                ContainerImageConfig {
                    image: image.clone(),
                    digest: pinned_digest.clone(),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            result.meta["manifest_digest"],
            Value::String(format!("sha256:{manifest_hex}"))
        );
        assert_eq!(result.meta.len(), 1);
        assert!(result.staged_path.join("oci-layout").exists());
        assert!(result.staged_path.join("index.json").exists());
        assert!(
            result
                .staged_path
                .join("blobs")
                .join("sha256")
                .join(&manifest_hex)
                .exists()
        );
        assert!(
            result
                .staged_path
                .join("blobs")
                .join("sha256")
                .join(&config_hex)
                .exists()
        );
        assert!(
            result
                .staged_path
                .join("blobs")
                .join("sha256")
                .join(&layer_hex)
                .exists()
        );
    }

    #[test]
    fn container_image_builder_rejects_non_empty_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("unexpected", resolved_binary_output(temp.path(), "bin-out"));

        let error = ContainerImageBuilder
            .build_typed(
                ContainerImageConfig {
                    image: "docker.io/library/ubuntu:20.04".to_string(),
                    digest: sample_digest().to_string(),
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn container_image_builder_rejects_invalid_digest() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = ContainerImageBuilder
            .build_typed(
                ContainerImageConfig {
                    image: "docker.io/library/ubuntu:20.04".to_string(),
                    digest: "sha256:short".to_string(),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = ContainerImageBuilder
            .build_erased(
                serde_json::json!({
                    "image": "docker.io/library/ubuntu:20.04",
                    "digest": sample_digest(),
                    "extra": true,
                }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    // -----------------------------------------------------------------------
    // Image builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn image_builder_bootstrap_mode_produces_oci_layout() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("in000", resolved_binary_output(temp.path(), "bin-out"));

        let result = ImageBuilder
            .build_typed(
                ImageConfig {
                    ref_name: None,
                    mode: None,
                },
                inputs,
                &mut cx,
            )
            .unwrap();

        assert!(
            result.meta.contains_key("manifest_digest"),
            "{:?}",
            result.meta
        );
        let digest = result.meta["manifest_digest"].as_str().unwrap();
        assert!(
            digest.starts_with("sha256:"),
            "manifest_digest should start with sha256:"
        );
        assert!(result.staged_path.join("oci-layout").exists());
        assert!(result.staged_path.join("index.json").exists());
    }

    #[test]
    fn image_builder_layered_mode_produces_oci_layout() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let base = resolved_base_image(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("base", base);
        inputs.insert("in000", resolved_binary_output(temp.path(), "bin-out"));

        let result = ImageBuilder
            .build_typed(
                ImageConfig {
                    ref_name: None,
                    mode: Some("layered".to_string()),
                },
                inputs,
                &mut cx,
            )
            .unwrap();

        let digest = result.meta["manifest_digest"].as_str().unwrap();
        assert!(digest.starts_with("sha256:"));
        assert!(result.staged_path.join("oci-layout").exists());
        assert!(result.staged_path.join("index.json").exists());
    }

    #[test]
    fn image_builder_layered_mode_has_more_blobs_than_base() {
        // Verify that the derived image has the base blobs plus new ones.
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let base_oci = create_test_oci_layout(temp.path(), "base-oci");
        let base_blob_count = fs::read_dir(base_oci.join("blobs").join("sha256"))
            .unwrap()
            .count();

        let base = BuilderInputObject {
            object_path: base_oci,
            meta: Map::new(),
        };
        let mut inputs = BuilderInputs::empty();
        inputs.insert("base", base);
        inputs.insert("in000", resolved_binary_output(temp.path(), "bin-out"));

        let result = ImageBuilder
            .build_typed(
                ImageConfig {
                    ref_name: None,
                    mode: Some("layered".to_string()),
                },
                inputs,
                &mut cx,
            )
            .unwrap();

        let new_blob_count = fs::read_dir(result.staged_path.join("blobs").join("sha256"))
            .unwrap()
            .count();
        // We should have base blobs + new manifest + new config + new layer = +3
        assert!(
            new_blob_count > base_blob_count,
            "layered image should have more blobs"
        );
    }

    #[test]
    fn image_builder_rejects_base_without_oci_layout_marker() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        // Create a directory without oci-layout file.
        let bad_base = temp.path().join("bad-base");
        fs::create_dir(&bad_base).unwrap();
        let base = BuilderInputObject {
            object_path: bad_base,
            meta: Map::new(),
        };
        let mut inputs = BuilderInputs::empty();
        inputs.insert("base", base);
        inputs.insert("in000", resolved_binary_output(temp.path(), "bin-out"));

        let error = ImageBuilder
            .build_typed(
                ImageConfig {
                    ref_name: None,
                    mode: Some("layered".to_string()),
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn image_builder_rejects_invalid_mode() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("in000", resolved_binary_output(temp.path(), "bin-out"));

        let error = ImageBuilder
            .build_typed(
                ImageConfig {
                    ref_name: None,
                    mode: Some("invalid".to_string()),
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn image_build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("in000", resolved_binary_output(temp.path(), "bin-out"));

        let error = ImageBuilder
            .build_erased(
                serde_json::json!({ "mode": "bootstrap", "extra": true }),
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }
}
