mod layer;
pub mod oci_extract;

use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    StagedBuildResult, TypedBuilder,
};
use mbuild_origin_oci_registry::oci;
pub use oci_extract::{OciExtractBuilder, OciExtractConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt;
use std::fs;
use std::path::Path;

const OCI_LAYOUT_SUBDIR: &str = "image";

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
pub struct ImageConfig {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    ref_name: Option<String>,
}

pub struct ImageBuilder;

static IMAGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "Image",
    required_inputs: &[],
    optional_inputs: &["base"],
    allow_extra_inputs: true,
};

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
        match mode {
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

        Ok(StagedBuildResult {
            staged_path,
            object_hash: None,
        })
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

fn run_layered_mode(
    staging_dir: &Path,
    base: &BuilderInputObject,
    binaries: &[BuilderInputObject],
    ref_name: Option<&str>,
) -> IResult<String> {
    let base_dir = &base.object_path;

    let base_manifest = oci::read_oci_manifest(base_dir)
        .map_err(|e| ImageError::InputResolutionFailed(e.to_string()))?;
    let base_config = oci::read_config(base_dir, &base_manifest)
        .map_err(|e| ImageError::InputResolutionFailed(e.to_string()))?;

    oci::init_layout(staging_dir).map_err(|e| ImageError::FsFailed(e.to_string()))?;
    oci::hardlink_layer_blobs(base_dir, staging_dir)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

    let binary_paths: Vec<&Path> = binaries.iter().map(|b| b.object_path.as_path()).collect();
    let layer =
        layer::create_layer(&binary_paths).map_err(|e| ImageError::BuildFailed(e.to_string()))?;
    let layer_desc = oci::write_blob(staging_dir, &layer.compressed, oci::MEDIA_TYPE_OCI_LAYER)
        .map_err(|e| ImageError::FsFailed(e.to_string()))?;

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
    use fsobj_hash::hash_path;
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
            object_hash: hash_path(&object_path).unwrap(),
            object_path,
        }
    }

    fn resolved_base_image(root: &Path) -> BuilderInputObject {
        let oci_dir = create_test_oci_layout(root, "base-image");
        BuilderInputObject {
            object_hash: hash_path(&oci_dir).unwrap(),
            object_path: oci_dir,
        }
    }

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

        assert!(result.staged_path.join("oci-layout").exists());
        assert!(result.staged_path.join("index.json").exists());
        assert_index_manifest_digest(&result.staged_path);
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

        assert!(result.staged_path.join("oci-layout").exists());
        assert!(result.staged_path.join("index.json").exists());
        assert_index_manifest_digest(&result.staged_path);
    }

    fn assert_index_manifest_digest(staged_path: &Path) {
        let index: Value =
            serde_json::from_slice(&fs::read(staged_path.join("index.json")).unwrap()).unwrap();
        let digest = index["manifests"][0]["digest"].as_str().unwrap();
        assert!(digest.starts_with("sha256:"));
    }

    #[test]
    fn image_builder_layered_mode_has_more_blobs_than_base() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let base_oci = create_test_oci_layout(temp.path(), "base-oci");
        let base_blob_count = fs::read_dir(base_oci.join("blobs").join("sha256"))
            .unwrap()
            .count();

        let base = BuilderInputObject {
            object_hash: hash_path(&base_oci).unwrap(),
            object_path: base_oci,
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
        assert!(new_blob_count > base_blob_count);
    }

    #[test]
    fn image_builder_rejects_base_without_oci_layout_marker() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let bad_base = temp.path().join("bad-base");
        fs::create_dir(&bad_base).unwrap();
        let base = BuilderInputObject {
            object_hash: hash_path(&bad_base).unwrap(),
            object_path: bad_base,
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
