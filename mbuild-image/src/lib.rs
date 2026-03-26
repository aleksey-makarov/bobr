use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    InputArity, InputSlot, ProducerInfo, StagedBuildResult, TypedBuilder, fsutil,
    load_container_image_descriptor,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

const KIND_BINARY_OUTPUT: &str = "binary-output";
const KIND_CONTAINER_IMAGE: &str = "container-image";
const DESCRIPTOR_SCHEMA: &str = "mbuild-container-image-object-v1";
const DESCRIPTOR_STORAGE: &str = "external-podman";
const GENERATED_IMAGE_PREFIX: &str = "localhost/mbuild-image";
#[cfg(test)]
const GENERATED_DIGEST: &str =
    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

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
            Self::InvalidConfig(message)
            | Self::InputResolutionFailed(message)
            | Self::BuildFailed(message)
            | Self::FsFailed(message) => message,
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

#[derive(Debug)]
struct ImportedImage {
    image_ref: String,
    image_id: String,
    image_digest: String,
}

pub struct ContainerImageBuilder;
pub struct ImageBuilder;

static CONTAINER_IMAGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "ContainerImage",
    inputs: &[] as &[InputSlot],
};

static IMAGE_INPUTS: &[InputSlot] = &[
    InputSlot {
        name: "base",
        arity: InputArity::Optional,
        allowed_kinds: &[KIND_CONTAINER_IMAGE],
    },
    InputSlot {
        name: "inputs",
        arity: InputArity::Many,
        allowed_kinds: &[KIND_BINARY_OUTPUT],
    },
];

static IMAGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "Image",
    inputs: IMAGE_INPUTS,
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
        validate_container_image_config(&config).map_err(map_container_image_error)?;
        if !inputs.is_empty() {
            return Err(BuilderError::ExecutionFailed(
                "ContainerImage builder does not accept input objects".to_string(),
            ));
        }

        fsutil::recreate_empty_dir_force(&cx.temp_root)
            .map_err(map_fsutil_error_to_container)
            .map_err(map_container_image_error)?;
        cx.log_event(
            BuildLogLevel::Info,
            "inspect",
            format!("inspecting local image '{}'", config.image),
        );

        let inspected =
            inspect_local_container_image_with_expected_digest(cx, &config.image, &config.digest)
                .map_err(map_container_image_error)?;
        let staged_path = write_descriptor(&cx.temp_root, &inspected.image_ref, &config.digest)
            .map_err(ContainerImageError::FsFailed)
            .map_err(map_container_image_error)?;

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
            attrs,
            staged_path,
        })
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
        let base = inputs.optional("base")?;
        let binaries = inputs.many("inputs")?;
        validate_image_config(&config, base, binaries).map_err(map_image_error)?;

        fsutil::recreate_empty_dir_force(&cx.temp_root)
            .map_err(map_fsutil_error_to_image)
            .map_err(map_image_error)?;

        let mode = effective_image_mode(&config, base).map_err(map_image_error)?;
        cx.log_event(
            BuildLogLevel::Info,
            "prepare",
            format!("building image in '{}' mode", mode),
        );
        let imported = match mode {
            "bootstrap" => run_bootstrap_mode(cx, binaries).map_err(map_image_error)?,
            "layered" => {
                let base = base.unwrap();
                run_layered_mode(cx, base, binaries).map_err(map_image_error)?
            }
            _ => unreachable!(),
        };

        let staged_path =
            write_descriptor(&cx.temp_root, &imported.image_ref, &imported.image_digest)
                .map_err(ImageError::FsFailed)
                .map_err(map_image_error)?;

        let mut attrs = Map::new();
        attrs.insert("mode".to_string(), Value::String(mode.to_string()));
        attrs.insert(
            "image_ref".to_string(),
            Value::String(imported.image_ref.clone()),
        );
        attrs.insert("image_id".to_string(), Value::String(imported.image_id));
        attrs.insert(
            "image_digest".to_string(),
            Value::String(imported.image_digest.clone()),
        );
        if let Some(base) = base {
            attrs.insert(
                "base_image_ref".to_string(),
                Value::String(resolve_image_ref(base).map_err(map_image_error)?),
            );
        }

        Ok(StagedBuildResult {
            kind: KIND_CONTAINER_IMAGE.to_string(),
            producer: ProducerInfo {
                builder: "image".to_string(),
            },
            attrs,
            staged_path,
        })
    }
}

fn validate_container_image_config(config: &ContainerImageConfig) -> CIResult<()> {
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

fn validate_image_config(
    config: &ImageConfig,
    base: Option<&BuilderInputObject>,
    binaries: &[BuilderInputObject],
) -> IResult<()> {
    if binaries.is_empty() {
        return Err(ImageError::InvalidConfig(
            "Image builder requires at least one binary-output input".to_string(),
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
        if !base.object_path.is_file() {
            return Err(ImageError::InputResolutionFailed(format!(
                "base container-image input must resolve to a file: {}",
                base.object_path.display()
            )));
        }
    }
    for binary in binaries {
        if !binary.object_path.is_dir() {
            return Err(ImageError::InputResolutionFailed(format!(
                "binary-output input must resolve to a directory: {}",
                binary.object_path.display()
            )));
        }
    }
    if matches!(config.mode.as_deref(), Some("layered")) && base.is_none() {
        return Err(ImageError::InvalidConfig(
            "image mode 'layered' requires a base container-image input".to_string(),
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
            "image mode 'bootstrap' is incompatible with a base container-image input".to_string(),
        )),
        (Some("layered"), false) => Err(ImageError::InvalidConfig(
            "image mode 'layered' requires a base container-image input".to_string(),
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

fn inspect_local_container_image_with_expected_digest(
    cx: &BuildContext,
    image: &str,
    digest: &str,
) -> CIResult<LocalImageInspect> {
    let inspected = inspect_local_image(cx, image)
        .map_err(|message| ContainerImageError::BuildFailed(message))?;
    if !inspected.image_ref.ends_with(&format!("@{digest}")) {
        return Err(ContainerImageError::BuildFailed(format!(
            "local image '{}' does not match required digest '{}'",
            image, digest
        )));
    }
    Ok(LocalImageInspect {
        image_ref: inspected.image_ref,
        image_id: inspected.image_id,
    })
}

fn inspect_generated_image(cx: &BuildContext, image_ref: &str) -> IResult<ImportedImage> {
    let inspected = inspect_local_image(cx, image_ref).map_err(ImageError::BuildFailed)?;
    let digest = inspected
        .image_ref
        .rsplit_once('@')
        .map(|(_, digest)| digest.to_string())
        .ok_or_else(|| {
            ImageError::BuildFailed(format!(
                "podman inspect did not return digest-qualified ref for image '{}'",
                image_ref
            ))
        })?;

    Ok(ImportedImage {
        image_ref: inspected.image_ref,
        image_id: inspected.image_id,
        image_digest: digest,
    })
}

fn inspect_local_image(cx: &BuildContext, image: &str) -> Result<LocalImageInspect, String> {
    let output = run_podman_command(cx, "podman-image-inspect", "podman image inspect", {
        let mut command = ProcessCommand::new("podman");
        command
            .arg("image")
            .arg("inspect")
            .arg(image)
            .arg("--format")
            .arg("json");
        command
    })?;

    if !output.status.success() {
        return Err(format!(
            "podman image '{}' is not available locally: {}",
            image,
            command_details(&output)
        ));
    }

    let records: Vec<PodmanImageInspectRecord> =
        serde_json::from_slice(&output.stdout).map_err(|error| {
            format!(
                "failed to parse podman inspect output for image '{}': {error}",
                image
            )
        })?;

    let record = records
        .first()
        .ok_or_else(|| format!("podman inspect returned no records for image '{}'", image))?;
    let image_ref = record.repo_digests.first().cloned().ok_or_else(|| {
        format!(
            "podman inspect returned no repo digests for image '{}'",
            image
        )
    })?;

    Ok(LocalImageInspect {
        image_ref,
        image_id: record.id.clone(),
    })
}

fn run_bootstrap_mode(cx: &BuildContext, binaries: &[BuilderInputObject]) -> IResult<ImportedImage> {
    let rootfs_dir = cx.temp_root.join("rootfs");
    let tar_path = cx.temp_root.join("rootfs.tar");
    fsutil::recreate_empty_dir_force(&rootfs_dir).map_err(map_fsutil_error_to_image)?;

    for binary in binaries {
        merge_directory(binary.object_path.as_path(), &rootfs_dir)?;
    }

    create_rootfs_tar(&rootfs_dir, &tar_path)?;
    let image_ref = generated_image_ref("bootstrap").map_err(map_fsutil_error_to_image)?;
    podman_import(cx, &tar_path, &image_ref)?;
    inspect_generated_image(cx, &image_ref)
}

fn run_layered_mode(
    cx: &BuildContext,
    base: &BuilderInputObject,
    binaries: &[BuilderInputObject],
) -> IResult<ImportedImage> {
    let base_ref = resolve_image_ref(base)?;
    let container_id = podman_create(cx, &base_ref)?;
    let result = (|| {
        for binary in binaries {
            podman_cp(cx, binary.object_path.as_path(), &container_id)?;
        }
        let image_ref = generated_image_ref("layered").map_err(map_fsutil_error_to_image)?;
        podman_commit(cx, &container_id, &image_ref)?;
        inspect_generated_image(cx, &image_ref)
    })();
    let _ = podman_rm(cx, &container_id);
    let _ = fsutil::recreate_empty_dir_force(&cx.temp_root.join("layered-work"))
        .map_err(map_fsutil_error_to_image);
    result
}

fn resolve_image_ref(image: &BuilderInputObject) -> IResult<String> {
    load_container_image_descriptor(&image.object_path)
        .map(|descriptor| descriptor.image_ref)
        .map_err(ImageError::InputResolutionFailed)
}

fn generated_image_ref(mode: &str) -> Result<String, fsutil::FsUtilError> {
    let now = fsutil::current_epoch_nanos()?;
    Ok(format!("{GENERATED_IMAGE_PREFIX}:{mode}-{now}"))
}

fn podman_import(cx: &BuildContext, tar_path: &Path, image_ref: &str) -> IResult<()> {
    let output = run_podman_command(cx, "podman-import", "podman import", {
        let mut command = ProcessCommand::new("podman");
        command.arg("import").arg(tar_path).arg(image_ref);
        command
    })
    .map_err(ImageError::BuildFailed)?;
    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "podman import failed: {}",
            command_details(&output)
        )));
    }
    Ok(())
}

fn podman_create(cx: &BuildContext, base_ref: &str) -> IResult<String> {
    let output = run_podman_command(cx, "podman-create", "podman create", {
        let mut command = ProcessCommand::new("podman");
        command.arg("create").arg(base_ref);
        command
    })
    .map_err(ImageError::BuildFailed)?;
    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "podman create failed: {}",
            command_details(&output)
        )));
    }
    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if container_id.is_empty() {
        return Err(ImageError::BuildFailed(
            "podman create did not return a container id".to_string(),
        ));
    }
    Ok(container_id)
}

fn podman_cp(cx: &BuildContext, binary_dir: &Path, container_id: &str) -> IResult<()> {
    let source = format!("{}/.", binary_dir.display());
    let destination = format!("{}:/", container_id);
    let output = run_podman_command(cx, "podman-cp", "podman cp", {
        let mut command = ProcessCommand::new("podman");
        command.arg("cp").arg(source).arg(destination);
        command
    })
    .map_err(ImageError::BuildFailed)?;
    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "podman cp failed: {}",
            command_details(&output)
        )));
    }
    Ok(())
}

fn podman_commit(cx: &BuildContext, container_id: &str, image_ref: &str) -> IResult<()> {
    let output = run_podman_command(cx, "podman-commit", "podman commit", {
        let mut command = ProcessCommand::new("podman");
        command.arg("commit").arg(container_id).arg(image_ref);
        command
    })
    .map_err(ImageError::BuildFailed)?;
    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "podman commit failed: {}",
            command_details(&output)
        )));
    }
    Ok(())
}

fn podman_rm(cx: &BuildContext, container_id: &str) -> IResult<()> {
    let output = run_podman_command(cx, "podman-rm", "podman rm", {
        let mut command = ProcessCommand::new("podman");
        command.arg("rm").arg(container_id);
        command
    })
    .map_err(ImageError::BuildFailed)?;
    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "podman rm failed: {}",
            command_details(&output)
        )));
    }
    Ok(())
}

fn merge_directory(source: &Path, destination: &Path) -> IResult<()> {
    for entry in fs::read_dir(source).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to read binary-output directory '{}': {error}",
            source.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to read directory entry in '{}': {error}",
                source.display()
            ))
        })?;
        let src_path = entry.path();
        let dst_path = destination.join(entry.file_name());
        copy_path_recursive(&src_path, &dst_path)?;
    }
    Ok(())
}

fn copy_path_recursive(source: &Path, destination: &Path) -> IResult<()> {
    let metadata = fs::symlink_metadata(source).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to inspect path '{}': {error}",
            source.display()
        ))
    })?;

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to read symlink '{}': {error}",
                source.display()
            ))
        })?;
        replace_path(destination)?;
        create_symlink(&target, destination)?;
        return Ok(());
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to create directory '{}': {error}",
                destination.display()
            ))
        })?;
        for entry in fs::read_dir(source).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to read directory '{}': {error}",
                source.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                ImageError::FsFailed(format!(
                    "failed to read directory entry in '{}': {error}",
                    source.display()
                ))
            })?;
            copy_path_recursive(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to create directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    replace_path(destination)?;
    fs::copy(source, destination).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to copy '{}' -> '{}': {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}

fn replace_path(path: &Path) -> IResult<()> {
    if !(path.exists() || path.is_symlink()) {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to inspect existing path '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to remove directory '{}': {error}",
                path.display()
            ))
        })?;
    } else {
        fs::remove_file(path).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to remove file '{}': {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> IResult<()> {
    std::os::unix::fs::symlink(target, link_path).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to create symlink '{}' -> '{}': {error}",
            link_path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link_path: &Path) -> IResult<()> {
    Err(ImageError::FsFailed(
        "symlink overlay for image builder is supported only on unix hosts".to_string(),
    ))
}

fn create_rootfs_tar(rootfs_dir: &Path, tar_path: &Path) -> IResult<()> {
    let tar_file = fs::File::create(tar_path).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to create rootfs tar '{}': {error}",
            tar_path.display()
        ))
    })?;
    let mut builder = tar::Builder::new(tar_file);
    builder.follow_symlinks(false);
    builder.append_dir_all(".", rootfs_dir).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to write rootfs tar '{}': {error}",
            tar_path.display()
        ))
    })?;
    builder.finish().map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to finalize rootfs tar '{}': {error}",
            tar_path.display()
        ))
    })?;
    Ok(())
}

fn write_descriptor(temp_root: &Path, image_ref: &str, digest: &str) -> Result<PathBuf, String> {
    let staged_path = temp_root.join("container-image.json");
    let payload = json!({
        "schema": DESCRIPTOR_SCHEMA,
        "storage": DESCRIPTOR_STORAGE,
        "image_ref": image_ref,
        "image_digest": digest,
    });
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| format!("failed to serialize container-image descriptor: {error}"))?;
    fsutil::write_atomic(&staged_path, &text).map_err(|error| error.to_string())?;
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

fn run_podman_command(
    cx: &BuildContext,
    label: &str,
    display_name: &str,
    mut command: ProcessCommand,
) -> Result<std::process::Output, String> {
    cx.log_event(
        BuildLogLevel::Info,
        label,
        format!("running {display_name}"),
    );
    let output = command
        .output()
        .map_err(|error| format!("failed to execute {display_name}: {error}"))?;
    let raw_log_path = write_podman_raw_log(cx, label, display_name, &output);

    if output.status.success() {
        cx.log_event_with_details(
            BuildLogLevel::Info,
            label,
            format!("{display_name} completed"),
            None,
            raw_log_path,
            Map::new(),
        );
    } else {
        cx.log_event_with_details(
            BuildLogLevel::Error,
            "command-fail",
            format!("{display_name} failed: {}", command_details(&output)),
            None,
            raw_log_path,
            Map::new(),
        );
    }

    Ok(output)
}

fn write_podman_raw_log(
    cx: &BuildContext,
    label: &str,
    display_name: &str,
    output: &std::process::Output,
) -> Option<PathBuf> {
    let log_content = format!(
        "command: {display_name}\nexit_code: {}\nstatus_success: {}\n\n=== stdout ===\n{}\n\n=== stderr ===\n{}\n",
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string()),
        output.status.success(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    cx.write_raw_log(label, &log_content)
}

fn map_fsutil_error_to_container(error: fsutil::FsUtilError) -> ContainerImageError {
    ContainerImageError::FsFailed(error.to_string())
}

fn map_fsutil_error_to_image(error: fsutil::FsUtilError) -> ImageError {
    ImageError::FsFailed(error.to_string())
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
    use mbuild_core::{
        BuildKey, Builder, BuilderInputObject, BuilderInputValue, BuilderInputs, ObjectHash,
        ResolvedInputValue, ResolvedInputs, ResolvedObject,
    };
    use std::env;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn build_context(root: &Path) -> BuildContext {
        BuildContext::with_noop_logger(
            root.to_path_buf(),
            root.join("builder"),
            root.join("tmp"),
            "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
            "Image",
            "image-test",
        )
    }

    fn install_fake_podman(dir: &Path, base_inspect_json: &str) {
        let script_path = dir.join("podman");
        let script = include_str!("../tests/assets/fake_podman_full.sh")
            .replace("__BASE_INSPECT_JSON__", base_inspect_json)
            .replace("__GENERATED_PREFIX__", GENERATED_IMAGE_PREFIX)
            .replace("__GENERATED_DIGEST__", GENERATED_DIGEST);
        fs::write(&script_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
        }
    }

    fn with_fake_podman<T>(base_inspect_json: &str, f: impl FnOnce() -> T) -> T {
        let _guard = env_lock().lock().unwrap();
        let temp = tempdir().unwrap();
        install_fake_podman(temp.path(), base_inspect_json);
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

    fn base_inspect_json() -> String {
        format!(
            "[{{\"Id\":\"sha256:imageid\",\"RepoDigests\":[\"docker.io/library/buildpack-deps@{}\"]}}]",
            sample_digest()
        )
    }

    fn resolved_binary_output(root: &Path, name: &str) -> BuilderInputObject {
        let object_path = root.join(name);
        fs::create_dir_all(&object_path).unwrap();
        fs::write(object_path.join("README.txt"), b"hello image\n").unwrap();
        BuilderInputObject { object_path }
    }

    fn resolved_base_image(root: &Path) -> BuilderInputObject {
        let object_path = root.join("base-image.json");
        let descriptor = serde_json::json!({
            "image_ref": format!("docker.io/library/buildpack-deps@{}", sample_digest()),
            "image_digest": sample_digest(),
        });
        fs::write(&object_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
        BuilderInputObject { object_path }
    }

    fn metadata_rich_base_image(
        root: &Path,
        descriptor_image_ref: &str,
        attrs_image_ref: &str,
    ) -> ResolvedObject {
        let object_path = root.join("base-image-metadata-rich.json");
        let descriptor = serde_json::json!({
            "image_ref": descriptor_image_ref,
            "image_digest": sample_digest(),
        });
        fs::write(&object_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();

        ResolvedObject {
            object_hash: "3333333333333333333333333333333333333333333333333333333333333333"
                .parse::<ObjectHash>()
                .unwrap(),
            build_key: "4444444444444444444444444444444444444444444444444444444444444444"
                .parse::<BuildKey>()
                .unwrap(),
            result_key: "4444444444444444444444444444444444444444444444444444444444444444"
                .parse::<BuildKey>()
                .unwrap(),
            kind: KIND_CONTAINER_IMAGE.to_string(),
            attrs: Map::from_iter([(
                "image_ref".to_string(),
                Value::String(attrs_image_ref.to_string()),
            )]),
            object_path,
        }
    }

    fn resolved_binary_output_internal(root: &Path, name: &str) -> ResolvedObject {
        let object = resolved_binary_output(root, name);
        ResolvedObject {
            object_hash: "1111111111111111111111111111111111111111111111111111111111111111"
                .parse::<ObjectHash>()
                .unwrap(),
            build_key: "2222222222222222222222222222222222222222222222222222222222222222"
                .parse::<BuildKey>()
                .unwrap(),
            result_key: "2222222222222222222222222222222222222222222222222222222222222222"
                .parse::<BuildKey>()
                .unwrap(),
            kind: KIND_BINARY_OUTPUT.to_string(),
            attrs: Map::new(),
            object_path: object.object_path,
        }
    }

    #[test]
    fn container_image_builder_writes_descriptor_and_attrs() {
        with_fake_podman(&base_inspect_json(), || {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = ContainerImageBuilder
                .build_typed(
                    ContainerImageConfig {
                        image: "docker.io/library/buildpack-deps:bookworm".to_string(),
                        digest: sample_digest().to_string(),
                    },
                    BuilderInputs::empty(),
                    &mut cx,
                )
                .unwrap();

            assert_eq!(result.kind, KIND_CONTAINER_IMAGE);
            assert_eq!(result.producer.builder, "container-image");
            assert_eq!(
                result.attrs["image"],
                Value::String("docker.io/library/buildpack-deps:bookworm".to_string())
            );
            assert_eq!(
                result.attrs["image_digest"],
                Value::String(sample_digest().to_string())
            );
            let descriptor: Value =
                serde_json::from_slice(&fs::read(&result.staged_path).unwrap()).unwrap();
            assert_eq!(
                descriptor["schema"],
                Value::String(DESCRIPTOR_SCHEMA.to_string())
            );
            assert_eq!(
                descriptor["storage"],
                Value::String(DESCRIPTOR_STORAGE.to_string())
            );
            assert_eq!(
                descriptor["image_digest"],
                Value::String(sample_digest().to_string())
            );
        });
    }

    #[test]
    fn image_builder_bootstrap_mode_writes_descriptor_and_attrs() {
        with_fake_podman(&base_inspect_json(), || {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let mut inputs = BuilderInputs::empty();
            inputs.insert("base", BuilderInputValue::Optional(None));
            inputs.insert(
                "inputs",
                BuilderInputValue::Many(vec![resolved_binary_output(temp.path(), "bin-out")]),
            );

            let result = ImageBuilder
                .build_typed(ImageConfig { mode: None }, inputs, &mut cx)
                .unwrap();

            assert_eq!(result.kind, KIND_CONTAINER_IMAGE);
            assert_eq!(result.producer.builder, "image");
            assert_eq!(result.attrs["mode"], Value::String("bootstrap".to_string()));
            assert_eq!(
                result.attrs["image_digest"],
                Value::String(GENERATED_DIGEST.to_string())
            );
            let descriptor: Value =
                serde_json::from_slice(&fs::read(&result.staged_path).unwrap()).unwrap();
            assert_eq!(
                descriptor["schema"],
                Value::String(DESCRIPTOR_SCHEMA.to_string())
            );
            assert_eq!(
                descriptor["image_digest"],
                Value::String(GENERATED_DIGEST.to_string())
            );
        });
    }

    #[test]
    fn image_builder_layered_mode_uses_base_image() {
        with_fake_podman(&base_inspect_json(), || {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let mut inputs = BuilderInputs::empty();
            inputs.insert(
                "base",
                BuilderInputValue::Optional(Some(resolved_base_image(temp.path()))),
            );
            inputs.insert(
                "inputs",
                BuilderInputValue::Many(vec![resolved_binary_output(temp.path(), "bin-out")]),
            );

            let result = ImageBuilder
                .build_typed(
                    ImageConfig {
                        mode: Some("layered".to_string()),
                    },
                    inputs,
                    &mut cx,
                )
                .unwrap();

            assert_eq!(result.attrs["mode"], Value::String("layered".to_string()));
            assert_eq!(
                result.attrs["base_image_ref"],
                Value::String(format!(
                    "docker.io/library/buildpack-deps@{}",
                    sample_digest()
                ))
            );
        });
    }

    #[test]
    fn image_builder_uses_descriptor_when_runtime_metadata_disagrees() {
        with_fake_podman(&base_inspect_json(), || {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let inputs = ResolvedInputs::new(std::collections::BTreeMap::from([
                (
                    "base".to_string(),
                    ResolvedInputValue::Optional(Some(metadata_rich_base_image(
                        temp.path(),
                        &format!("docker.io/library/buildpack-deps@{}", sample_digest()),
                        "docker.io/library/wrong@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    ))),
                ),
                (
                    "inputs".to_string(),
                    ResolvedInputValue::Many(vec![resolved_binary_output_internal(
                        temp.path(),
                        "bin-out",
                    )]),
                ),
            ]))
            .into_builder_inputs();

            let result = ImageBuilder
                .build_typed(
                    ImageConfig {
                        mode: Some("layered".to_string()),
                    },
                    inputs,
                    &mut cx,
                )
                .unwrap();

            assert_eq!(
                result.attrs["base_image_ref"],
                Value::String(format!(
                    "docker.io/library/buildpack-deps@{}",
                    sample_digest()
                ))
            );
        });
    }

    #[test]
    fn container_image_builder_rejects_non_empty_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("base", BuilderInputValue::Many(vec![]));

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
                BuilderInputs::empty(),
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
        inputs.insert("base", BuilderInputValue::Optional(None));
        inputs.insert(
            "inputs",
            BuilderInputValue::Many(vec![resolved_binary_output(temp.path(), "bin-out")]),
        );

        let error = ImageBuilder
            .build_erased(
                json!({ "mode": "bootstrap", "extra": true }),
                inputs,
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
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn image_builder_rejects_invalid_mode() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("base", BuilderInputValue::Optional(None));
        inputs.insert(
            "inputs",
            BuilderInputValue::Many(vec![resolved_binary_output(temp.path(), "bin-out")]),
        );

        let error = ImageBuilder
            .build_typed(
                ImageConfig {
                    mode: Some("invalid".to_string()),
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn image_builder_reports_podman_import_failure() {
        with_fake_podman(&base_inspect_json(), || {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let mut inputs = BuilderInputs::empty();
            inputs.insert("base", BuilderInputValue::Optional(None));
            inputs.insert(
                "inputs",
                BuilderInputValue::Many(vec![resolved_binary_output(temp.path(), "bin-out")]),
            );

            unsafe { env::set_var("MBUILD_TEST_IMAGE_IMPORT_FAIL", "1") };
            let error = ImageBuilder
                .build_typed(ImageConfig { mode: None }, inputs, &mut cx)
                .unwrap_err();
            unsafe { env::remove_var("MBUILD_TEST_IMAGE_IMPORT_FAIL") };

            assert!(matches!(error, BuilderError::ExecutionFailed(_)));
            let message = error.to_string();
            assert!(message.contains("podman import"), "{message}");
        });
    }
}
