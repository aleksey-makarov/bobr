use mbuild_core::{
    BuildContext, BuilderError, BuilderSpec, InputSlot, ProducerInfo, ResolvedInputs,
    StagedBuildResult, TypedBuilder, fsutil,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

const KIND_CONTAINER_IMAGE: &str = "container-image";
const DESCRIPTOR_SCHEMA: &str = "mbuild-container-image-object-v1";
const DESCRIPTOR_STORAGE: &str = "external-podman";

#[derive(Debug)]
enum ContainerImageError {
    InvalidConfig(String),
    BuildFailed(String),
    FsFailed(String),
}

impl ContainerImageError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message) | Self::BuildFailed(message) | Self::FsFailed(message) => {
                message
            }
        }
    }
}

impl fmt::Display for ContainerImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type CIResult<T> = Result<T, ContainerImageError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerImageConfig {
    image: String,
    digest: String,
}

#[derive(Debug, Deserialize)]
struct PodmanImageInspectRecord {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "RepoDigests", default)]
    repo_digests: Vec<String>,
}

#[derive(Debug)]
struct LocalImageInspect {
    image_ref: String,
    image_id: String,
}

pub struct ContainerImageBuilder;

static CONTAINER_IMAGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "ContainerImage",
    inputs: &[] as &[InputSlot],
};

impl TypedBuilder for ContainerImageBuilder {
    type Config = ContainerImageConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &CONTAINER_IMAGE_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: ResolvedInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_config(&config).map_err(map_error)?;
        if !inputs.is_empty() {
            return Err(BuilderError::ExecutionFailed(
                "ContainerImage builder does not accept input objects".to_string(),
            ));
        }

        fsutil::recreate_empty_dir_force(&cx.temp_root)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;

        let inspected = inspect_local_container_image(&config.image, &config.digest).map_err(map_error)?;
        let staged_path = write_descriptor(&cx.temp_root, &inspected, &config.digest).map_err(map_error)?;

        let mut attrs = Map::new();
        attrs.insert("image".to_string(), Value::String(config.image));
        attrs.insert(
            "image_ref".to_string(),
            Value::String(inspected.image_ref.clone()),
        );
        attrs.insert("image_id".to_string(), Value::String(inspected.image_id));
        attrs.insert(
            "image_digest".to_string(),
            Value::String(config.digest.clone()),
        );

        Ok(StagedBuildResult {
            kind: KIND_CONTAINER_IMAGE.to_string(),
            producer: ProducerInfo {
                builder: "container-image".to_string(),
            },
            input_object_hashes: vec![],
            attrs,
            staged_path,
        })
    }
}

fn validate_config(config: &ContainerImageConfig) -> CIResult<()> {
    if config.image.trim().is_empty() {
        return Err(ContainerImageError::InvalidConfig(
            "image must not be empty".to_string(),
        ));
    }
    if !is_valid_sha256_digest(&config.digest) {
        return Err(ContainerImageError::InvalidConfig(format!(
            "invalid digest '{}'; expected format: sha256:<64 hex chars>",
            config.digest
        )));
    }
    Ok(())
}

fn is_valid_sha256_digest(value: &str) -> bool {
    const PREFIX: &str = "sha256:";
    if !value.starts_with(PREFIX) {
        return false;
    }
    let hex = &value[PREFIX.len()..];
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn inspect_local_container_image(image: &str, digest: &str) -> CIResult<LocalImageInspect> {
    let output = ProcessCommand::new("podman")
        .arg("image")
        .arg("inspect")
        .arg(image)
        .arg("--format")
        .arg("json")
        .output()
        .map_err(|error| {
            ContainerImageError::BuildFailed(format!(
                "failed to execute podman image inspect: {error}"
            ))
        })?;

    if !output.status.success() {
        return Err(ContainerImageError::BuildFailed(format!(
            "podman image '{}' with digest '{}' is not available locally: {}",
            image,
            digest,
            command_details(&output)
        )));
    }

    let records: Vec<PodmanImageInspectRecord> = serde_json::from_slice(&output.stdout).map_err(|error| {
        ContainerImageError::BuildFailed(format!(
            "failed to parse podman inspect output for image '{}': {error}",
            image
        ))
    })?;

    let record = records.first().ok_or_else(|| {
        ContainerImageError::BuildFailed(format!(
            "podman inspect returned no records for image '{}'",
            image
        ))
    })?;

    let expected_suffix = format!("@{digest}");
    let matching_ref = record
        .repo_digests
        .iter()
        .find(|value| value.ends_with(&expected_suffix))
        .cloned()
        .ok_or_else(|| {
            ContainerImageError::BuildFailed(format!(
                "local image '{}' does not match required digest '{}'",
                image, digest
            ))
        })?;

    Ok(LocalImageInspect {
        image_ref: matching_ref,
        image_id: record.id.clone(),
    })
}

fn write_descriptor(temp_root: &Path, inspected: &LocalImageInspect, digest: &str) -> CIResult<PathBuf> {
    let staged_path = temp_root.join("container-image.json");
    let payload = json!({
        "schema": DESCRIPTOR_SCHEMA,
        "storage": DESCRIPTOR_STORAGE,
        "image_ref": inspected.image_ref,
        "image_digest": digest,
    });
    let text = serde_json::to_string_pretty(&payload).map_err(|error| {
        ContainerImageError::FsFailed(format!(
            "failed to serialize container-image descriptor: {error}"
        ))
    })?;
    fsutil::write_atomic(&staged_path, &text).map_err(map_fsutil_error)?;
    Ok(staged_path)
}

fn command_details(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "no output".to_string(),
        (false, true) => format!("stdout: {stdout}"),
        (true, false) => format!("stderr: {stderr}"),
        (false, false) => format!("stdout: {stdout}; stderr: {stderr}"),
    }
}

fn map_fsutil_error(error: fsutil::FsUtilError) -> ContainerImageError {
    ContainerImageError::FsFailed(error.to_string())
}

fn map_error(error: ContainerImageError) -> BuilderError {
    match error {
        ContainerImageError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        ContainerImageError::BuildFailed(message) | ContainerImageError::FsFailed(message) => {
            BuilderError::ExecutionFailed(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::Builder;
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn build_context(root: &Path) -> BuildContext {
        BuildContext {
            workspace_root: root.to_path_buf(),
            builder_root: root.join("builder"),
            temp_root: root.join("tmp"),
        }
    }

    fn install_fake_podman(dir: &Path, inspect_json: &str) {
        let script_path = dir.join("podman");
        fs::write(
            &script_path,
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\nif [ \"$1\" = image ] && [ \"$2\" = inspect ]; then\n  cat <<'JSON'\n{inspect_json}\nJSON\nelse\n  echo unexpected podman invocation: \"$@\" >&2\n  exit 1\nfi\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
        }
    }

    fn with_fake_podman<T>(inspect_json: &str, f: impl FnOnce() -> T) -> T {
        let _guard = env_lock().lock().unwrap();
        let temp = tempdir().unwrap();
        install_fake_podman(temp.path(), inspect_json);
        let previous_path = env::var_os("PATH");
        let new_path = match &previous_path {
            Some(existing) => {
                let mut joined = temp.path().as_os_str().to_os_string();
                joined.push(":");
                joined.push(existing);
                joined
            }
            None => temp.path().as_os_str().to_os_string(),
        };
        unsafe { env::set_var("PATH", &new_path) };
        let result = f();
        match previous_path {
            Some(path) => unsafe { env::set_var("PATH", path) },
            None => unsafe { env::remove_var("PATH") },
        }
        result
    }

    fn sample_digest() -> &'static str {
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }

    #[test]
    fn container_image_builder_writes_descriptor_and_attrs() {
        with_fake_podman(
            &format!(
                "[{{\"Id\":\"sha256:imageid\",\"RepoDigests\":[\"docker.io/library/buildpack-deps@{}\"]}}]",
                sample_digest()
            ),
            || {
                let temp = tempdir().unwrap();
                let mut cx = build_context(temp.path());
                let result = ContainerImageBuilder
                    .build_typed(
                        ContainerImageConfig {
                            image: "docker.io/library/buildpack-deps:bookworm".to_string(),
                            digest: sample_digest().to_string(),
                        },
                        ResolvedInputs::empty(),
                        &mut cx,
                    )
                    .unwrap();

                assert_eq!(result.kind, KIND_CONTAINER_IMAGE);
                assert_eq!(result.producer.builder, "container-image");
                assert_eq!(result.input_object_hashes.len(), 0);
                assert_eq!(result.attrs["image"], Value::String("docker.io/library/buildpack-deps:bookworm".to_string()));
                assert_eq!(result.attrs["image_digest"], Value::String(sample_digest().to_string()));
                let descriptor: Value = serde_json::from_slice(&fs::read(&result.staged_path).unwrap()).unwrap();
                assert_eq!(descriptor["schema"], Value::String(DESCRIPTOR_SCHEMA.to_string()));
                assert_eq!(descriptor["storage"], Value::String(DESCRIPTOR_STORAGE.to_string()));
                assert_eq!(descriptor["image_digest"], Value::String(sample_digest().to_string()));
            },
        );
    }

    #[test]
    fn container_image_builder_rejects_non_empty_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("base", mbuild_core::ResolvedInputValue::Many(vec![]));

        let error = ContainerImageBuilder
            .build_typed(
                ContainerImageConfig {
                    image: "docker.io/library/buildpack-deps:bookworm".to_string(),
                    digest: sample_digest().to_string(),
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = ContainerImageBuilder
            .build_erased(
                json!({
                    "image": "docker.io/library/buildpack-deps:bookworm",
                    "digest": sample_digest(),
                    "extra": true,
                }),
                ResolvedInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn container_image_builder_rejects_invalid_digest() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = ContainerImageBuilder
            .build_typed(
                ContainerImageConfig {
                    image: "docker.io/library/buildpack-deps:bookworm".to_string(),
                    digest: "sha256:short".to_string(),
                },
                ResolvedInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }
}
