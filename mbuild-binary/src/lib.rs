use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    StagedBuildResult, TypedBuilder, fsutil,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
    fn getegid() -> u32;
}

const BUILD_DIR_NAME: &str = "build";
const OUTPUT_DIR_NAME: &str = "out";
const CONFIG_DIR_NAME: &str = "config";
const INPUT_MOUNT_ROOT: &str = "/__mbuild/inputs";
const CONFIG_MOUNT_PATH: &str = "/__mbuild/config";
const CONFIG_ENV_VAR: &str = "MBUILD_CONFIG_DIR";
const BUILD_DIR_ENV_VAR: &str = "MBUILD_BUILD_DIR";
const OUT_DIR_ENV_VAR: &str = "MBUILD_OUT_DIR";
const STEP_NAME_ENV_VAR: &str = "MBUILD_STEP_NAME";
const BUILD_DIR_MOUNT_PATH: &str = "/__mbuild/build";
const OUT_DIR_MOUNT_PATH: &str = "/__mbuild/out";

#[derive(Debug)]
enum BinaryError {
    InvalidConfig(String),
    InputResolutionFailed(String),
    PodmanFailed(String),
    BuildFailed(String),
    FsFailed(String),
}

impl BinaryError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::InputResolutionFailed(message)
            | Self::PodmanFailed(message)
            | Self::BuildFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for BinaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type BResult<T> = Result<T, BinaryError>;

#[derive(Debug)]
struct CleanupError {
    message: String,
    raw_log_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryConfig {
    #[serde(default)]
    script_config: Option<Value>,
    steps: Vec<BuildStep>,
    #[serde(default)]
    install: Option<InstallMeta>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StepUser {
    BuildUser,
    Root,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildStep {
    name: String,
    run_as: StepUser,
    cwd: String,
    argv: Vec<String>,
    #[serde(default)]
    env: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallMeta {
    rules: Vec<InstallRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallRule {
    path: String,
    attrs: InstallAttrs,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallAttrs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    regular_file_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executable_file_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symlink_mode: Option<u32>,
}

struct InputExecution {
    config_host_path: PathBuf,
}

struct ContainerExecution {
    image_ref: String,
}

struct ContainerInstance {
    id: String,
}

pub struct BinaryBuilder;

static BINARY_SPEC: BuilderSpec = BuilderSpec {
    tag: "Binary",
    required_inputs: &["image"],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for BinaryBuilder {
    type Config = BinaryConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &BINARY_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_config(&config).map_err(map_error)?;
        let image = inputs.required("image")?;
        let named_inputs = collect_named_inputs(&inputs).map_err(map_error)?;
        validate_step_interpolations(&config.steps, &named_inputs).map_err(map_error)?;

        let output_path = cx.temp_dir.join(OUTPUT_DIR_NAME);
        fsutil::recreate_empty_dir_force(&output_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let build_path = cx.temp_dir.join(BUILD_DIR_NAME);
        fsutil::recreate_empty_dir_force(&build_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let config_path = cx.temp_dir.join(CONFIG_DIR_NAME);
        fsutil::recreate_empty_dir_force(&config_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        write_script_config(&config_path, config.script_config.as_ref()).map_err(map_error)?;

        let input_execution =
            resolve_input_execution(&config_path, &named_inputs).map_err(map_error)?;
        let container_execution = resolve_container_execution(image, cx).map_err(map_error)?;
        cx.log_event(
            BuildLogLevel::Info,
            "prepare",
            format!(
                "resolved container image, {} input(s), and config dir",
                named_inputs.len()
            ),
        );

        let build_result = run_container_build(
            &container_execution,
            &input_execution,
            &config.steps,
            &named_inputs,
            &build_path,
            &output_path,
            cx,
        );

        if let Err(error) = build_result {
            return Err(map_error(error));
        }

        let mut meta = Map::new();
        let install = config.install.unwrap_or_else(default_install_meta);
        meta.insert(
            "install".to_string(),
            serde_json::to_value(&install).map_err(|error| {
                map_error(BinaryError::BuildFailed(format!(
                    "failed to serialize install metadata: {error}"
                )))
            })?,
        );

        Ok(StagedBuildResult {
            meta,
            staged_path: output_path,
        })
    }
}

fn validate_config(config: &BinaryConfig) -> BResult<()> {
    validate_script_config(config.script_config.as_ref())?;
    validate_steps(&config.steps)?;
    validate_install(config.install.as_ref())?;
    Ok(())
}

fn validate_steps(steps: &[BuildStep]) -> BResult<()> {
    if steps.is_empty() {
        return Err(BinaryError::InvalidConfig(
            "steps must contain at least one step".to_string(),
        ));
    }

    for (index, step) in steps.iter().enumerate() {
        if step.name.trim().is_empty() {
            return Err(BinaryError::InvalidConfig(format!(
                "steps[{index}].name must not be empty"
            )));
        }
        if step.cwd.trim().is_empty() {
            return Err(BinaryError::InvalidConfig(format!(
                "steps[{index}].cwd must not be empty"
            )));
        }
        if step.argv.is_empty() {
            return Err(BinaryError::InvalidConfig(format!(
                "steps[{index}].argv must not be empty"
            )));
        }
        for (arg_index, arg) in step.argv.iter().enumerate() {
            if arg.is_empty() {
                return Err(BinaryError::InvalidConfig(format!(
                    "steps[{index}].argv[{arg_index}] must not be empty"
                )));
            }
        }
        validate_step_env(&step.env, &format!("steps[{index}].env"))?;
    }

    Ok(())
}

fn validate_step_env(env: &Map<String, Value>, path: &str) -> BResult<()> {
    for (key, value) in env {
        validate_script_config_key(key, path)?;
        if !matches!(value, Value::String(_)) {
            return Err(BinaryError::InvalidConfig(format!(
                "{path}.{key} must be a string"
            )));
        }
    }
    Ok(())
}

fn validate_install(install: Option<&InstallMeta>) -> BResult<()> {
    if let Some(install) = install
        && install.rules.is_empty()
    {
        return Err(BinaryError::InvalidConfig(
            "install.rules must contain at least one rule".to_string(),
        ));
    }
    Ok(())
}

fn default_install_meta() -> InstallMeta {
    InstallMeta {
        rules: vec![InstallRule {
            path: "**".to_string(),
            attrs: InstallAttrs {
                uid: Some(0),
                gid: Some(0),
                directory_mode: Some(0o755),
                regular_file_mode: Some(0o644),
                executable_file_mode: Some(0o755),
                symlink_mode: Some(0o777),
            },
        }],
    }
}

fn validate_input_name(name: &str) -> BResult<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(BinaryError::InvalidConfig(
            "input name must not be empty".to_string(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(BinaryError::InvalidConfig(format!(
            "input name '{name}' must start with an ASCII letter or underscore"
        )));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(BinaryError::InvalidConfig(format!(
            "input name '{name}' must contain only ASCII letters, digits, and underscores"
        )));
    }
    Ok(())
}

fn input_mount_path(name: &str) -> String {
    format!("{INPUT_MOUNT_ROOT}/{name}")
}

fn collect_named_inputs(inputs: &BuilderInputs) -> BResult<Vec<(String, BuilderInputObject)>> {
    let mut named = Vec::new();
    for (name, object) in inputs.extras(&BINARY_SPEC) {
        validate_input_name(name)?;
        if matches!(name, "build" | "out" | "config") {
            return Err(BinaryError::InvalidConfig(format!(
                "input name '{name}' conflicts with a reserved Binary interpolation variable"
            )));
        }
        named.push((name.to_string(), object.clone()));
    }
    Ok(named)
}

fn resolve_input_execution(
    script_config_dir: &Path,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<InputExecution> {
    for (_, input) in inputs {
        if !input.object_path.is_dir() && !input.object_path.is_file() {
            return Err(BinaryError::InputResolutionFailed(format!(
                "binary input must resolve to a file or directory: {}",
                input.object_path.display()
            )));
        }
    }

    Ok(InputExecution {
        config_host_path: script_config_dir.to_path_buf(),
    })
}

fn resolve_container_execution(
    image: &BuilderInputObject,
    cx: &BuildContext,
) -> BResult<ContainerExecution> {
    if !image.object_path.is_dir() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "image input must resolve to a directory: {}",
            image.object_path.display()
        )));
    }
    if !image.object_path.join("oci-layout").exists() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "image input is not a valid OCI layout directory: {}",
            image.object_path.display()
        )));
    }

    let config_digest = read_config_digest_from_oci_layout(&image.object_path)
        .map_err(BinaryError::InputResolutionFailed)?;

    // Check if the image is already loaded in podman (by config digest = OCI image ID).
    let exists = is_image_loaded_in_podman(&config_digest, cx)?;
    if !exists {
        load_oci_to_podman(&image.object_path, cx)?;
    }

    Ok(ContainerExecution {
        image_ref: config_digest,
    })
}
/// Read the config blob digest from an OCI layout directory.
fn read_config_digest_from_oci_layout(oci_dir: &std::path::Path) -> Result<String, String> {
    let index_bytes = std::fs::read(oci_dir.join("index.json"))
        .map_err(|e| format!("failed to read index.json in '{}': {e}", oci_dir.display()))?;
    let index: serde_json::Value = serde_json::from_slice(&index_bytes)
        .map_err(|e| format!("failed to parse index.json: {e}"))?;
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .ok_or_else(|| "index.json: missing manifests[0].digest".to_string())?
        .to_string();

    let (alg, hex) = manifest_digest
        .split_once(':')
        .ok_or_else(|| format!("invalid manifest digest '{manifest_digest}'"))?;
    let blob_path = oci_dir.join("blobs").join(alg).join(hex);
    let manifest_bytes = std::fs::read(&blob_path).map_err(|e| {
        format!(
            "failed to read manifest blob '{}': {e}",
            blob_path.display()
        )
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("failed to parse manifest: {e}"))?;
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| "manifest: missing config.digest".to_string())?
        .to_string();
    Ok(config_digest)
}

/// Check whether the image with the given config digest (OCI image ID) is
/// already loaded in podman.
fn is_image_loaded_in_podman(config_digest: &str, cx: &BuildContext) -> BResult<bool> {
    let mut cmd = ProcessCommand::new("podman");
    cmd.arg("image").arg("exists").arg(config_digest);
    cx.log_event(
        BuildLogLevel::Info,
        "podman-image-exists",
        format!("checking podman for image {config_digest}"),
    );
    let output = cmd.output().map_err(|e| {
        BinaryError::PodmanFailed(format!("failed to run podman image exists: {e}"))
    })?;
    Ok(output.status.success())
}

/// Create a tar archive of an OCI layout directory and load it into podman.
fn load_oci_to_podman(oci_dir: &std::path::Path, cx: &BuildContext) -> BResult<()> {
    let tar_path = cx.temp_dir.join("oci-load.tar");
    {
        let file = std::fs::File::create(&tar_path).map_err(|e| {
            BinaryError::FsFailed(format!(
                "failed to create OCI tar '{}': {e}",
                tar_path.display()
            ))
        })?;
        let mut builder = tar::Builder::new(file);
        builder.follow_symlinks(false);
        builder
            .append_dir_all(".", oci_dir)
            .map_err(|e| BinaryError::FsFailed(format!("failed to write OCI tar: {e}")))?;
        builder
            .finish()
            .map_err(|e| BinaryError::FsFailed(format!("failed to finalize OCI tar: {e}")))?;
    }

    cx.log_event(
        BuildLogLevel::Info,
        "podman-load",
        format!("loading OCI image from '{}'", oci_dir.display()),
    );
    let mut cmd = ProcessCommand::new("podman");
    // ignore_chown_errors: rootless podman without /etc/subuid cannot map all
    // uid/gid values (e.g. gid=42 for the shadow group in Debian images).
    // Files retain the current user's ownership instead — acceptable for builds.
    cmd.arg("--storage-opt").arg("ignore_chown_errors=true");
    cmd.arg("load").arg("--input").arg(&tar_path);
    // Clear TMPDIR: podman uses it for temp files when processing oci-archive,
    // and a stale value (e.g. from an expired nix-shell) causes load to fail.
    cmd.env_remove("TMPDIR");
    let output = cmd
        .output()
        .map_err(|e| BinaryError::PodmanFailed(format!("failed to run podman load: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(BinaryError::PodmanFailed(format!(
            "podman load failed: {stderr}"
        )));
    }
    Ok(())
}

fn run_container_build(
    container: &ContainerExecution,
    input_execution: &InputExecution,
    steps: &[BuildStep],
    inputs: &[(String, BuilderInputObject)],
    build_path: &Path,
    output_path: &Path,
    cx: &BuildContext,
) -> BResult<()> {
    let (uid, gid) = current_uid_gid();
    let instance = create_container(
        container,
        input_execution,
        inputs,
        build_path,
        output_path,
        uid,
        gid,
        cx,
    )?;
    let build_result = (|| {
        start_container(&instance, cx)?;
        prepare_container_workspace(&instance, uid, gid, cx)?;
        for step in steps {
            exec_step(&instance, step, inputs, cx)?;
        }
        export_container_output(&instance, output_path, cx)?;
        if !output_path.is_dir() {
            return Err(BinaryError::BuildFailed(format!(
                "binary builder did not produce output directory '{}'",
                output_path.display()
            )));
        }
        Ok(())
    })();
    let cleanup_result = remove_container(&instance, cx);
    match (build_result, cleanup_result) {
        (Err(error), Err(cleanup_error)) => {
            log_cleanup_warning(cx, &instance.id, &cleanup_error, false);
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => {
            log_cleanup_warning(cx, &instance.id, &cleanup_error, true);
            Ok(())
        }
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn current_uid_gid() -> (u32, u32) {
    #[cfg(unix)]
    unsafe {
        (geteuid(), getegid())
    }
    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

fn interpolate_step_string(
    value: &str,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<String> {
    let mut rendered = String::new();
    let mut index = 0;

    while index < value.len() {
        let rest = &value[index..];
        if let Some(after_escape) = rest.strip_prefix("@@{") {
            let Some(end) = after_escape.find('}') else {
                return Err(BinaryError::InvalidConfig(format!(
                    "unterminated interpolation escape in '{value}'"
                )));
            };
            let key = &after_escape[..end];
            validate_interpolation_name(key, value, true)?;
            rendered.push_str("@{");
            rendered.push_str(key);
            rendered.push('}');
            index += 3 + end + 1;
            continue;
        }
        if let Some(after_start) = rest.strip_prefix("@{") {
            let Some(end) = after_start.find('}') else {
                return Err(BinaryError::InvalidConfig(format!(
                    "unterminated interpolation in '{value}'"
                )));
            };
            let key = &after_start[..end];
            validate_interpolation_name(key, value, false)?;
            let replacement = interpolation_value(key, inputs)?;
            rendered.push_str(&replacement);
            index += 2 + end + 1;
            continue;
        }

        let ch = rest.chars().next().expect("rest is non-empty");
        rendered.push(ch);
        index += ch.len_utf8();
    }

    Ok(rendered)
}

fn validate_interpolation_name(key: &str, value: &str, escaped: bool) -> BResult<()> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(BinaryError::InvalidConfig(format!(
            "invalid {} in '{value}'",
            if escaped {
                "interpolation escape '@@{}'"
            } else {
                "interpolation variable '@{}'"
            }
        )));
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(BinaryError::InvalidConfig(format!(
            "invalid {} in '{value}'",
            if escaped {
                format!("interpolation escape '@@{{{key}}}'")
            } else {
                format!("interpolation variable '@{{{key}}}'")
            }
        )));
    }
    Ok(())
}

fn interpolation_value(key: &str, inputs: &[(String, BuilderInputObject)]) -> BResult<String> {
    match key {
        "build" => Ok(BUILD_DIR_MOUNT_PATH.to_string()),
        "out" => Ok(OUT_DIR_MOUNT_PATH.to_string()),
        "config" => Ok(CONFIG_MOUNT_PATH.to_string()),
        _ => inputs
            .iter()
            .find_map(|(name, _)| {
                if name == key {
                    Some(input_mount_path(name))
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                BinaryError::InvalidConfig(format!("unknown interpolation variable '@{{{key}}}'"))
            }),
    }
}

fn resolve_step_cwd(step: &BuildStep, inputs: &[(String, BuilderInputObject)]) -> BResult<String> {
    let cwd = interpolate_step_string(&step.cwd, inputs)?;
    if cwd.is_empty() || !cwd.starts_with('/') {
        return Err(BinaryError::InvalidConfig(format!(
            "step '{}' resolved cwd must be an absolute path, got '{}'",
            step.name, cwd
        )));
    }
    Ok(cwd)
}

fn resolve_step_argv(
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<Vec<String>> {
    step.argv
        .iter()
        .map(|arg| interpolate_step_string(arg, inputs))
        .collect()
}

fn resolve_step_env(
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<Vec<(String, String)>> {
    let mut rendered = Vec::new();
    for (key, value) in &step.env {
        let string_value = value.as_str().ok_or_else(|| {
            BinaryError::InvalidConfig(format!(
                "step '{}' env key '{}' must be a string",
                step.name, key
            ))
        })?;
        rendered.push((key.clone(), interpolate_step_string(string_value, inputs)?));
    }
    Ok(rendered)
}

fn validate_step_interpolations(
    steps: &[BuildStep],
    inputs: &[(String, BuilderInputObject)],
) -> BResult<()> {
    for step in steps {
        let _ = resolve_step_cwd(step, inputs)?;
        let _ = resolve_step_argv(step, inputs)?;
        let _ = resolve_step_env(step, inputs)?;
    }
    Ok(())
}

fn create_container(
    container: &ContainerExecution,
    input_execution: &InputExecution,
    inputs: &[(String, BuilderInputObject)],
    _build_path: &Path,
    _output_path: &Path,
    uid: u32,
    gid: u32,
    cx: &BuildContext,
) -> BResult<ContainerInstance> {
    cx.log_event(
        BuildLogLevel::Info,
        "podman-create",
        format!(
            "creating build container from image '{}'",
            container.image_ref
        ),
    );
    let mut process = ProcessCommand::new("podman");
    process
        .arg("create")
        .arg("--network=none")
        .arg("--userns=keep-id")
        .arg("--user")
        .arg(format!("{uid}:{gid}"));

    for (name, input) in inputs {
        let mount_path = input_mount_path(name);
        let mount_spec = if input.object_path.is_dir() {
            format!("{}:{mount_path}:O", input.object_path.display())
        } else {
            format!("{}:{mount_path}:ro", input.object_path.display())
        };
        process.arg("--volume").arg(mount_spec);
    }

    process.arg("--volume").arg(format!(
        "{}:{}:ro",
        input_execution.config_host_path.display(),
        CONFIG_MOUNT_PATH
    ));

    process
        .arg("--env")
        .arg(format!("{CONFIG_ENV_VAR}={CONFIG_MOUNT_PATH}"))
        .arg("--env")
        .arg(format!("{BUILD_DIR_ENV_VAR}={BUILD_DIR_MOUNT_PATH}"))
        .arg("--env")
        .arg(format!("{OUT_DIR_ENV_VAR}={OUT_DIR_MOUNT_PATH}"));
    process
        .arg(&container.image_ref)
        .arg("sleep")
        .arg("infinity");

    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!("failed to execute podman create: {error}"))
    })?;
    let log_path = write_command_log(cx, "podman-create", &container.image_ref, inputs, &output);

    if !output.status.success() {
        return Err(command_failure(
            "podman-create",
            "podman create",
            &output,
            log_path,
        ));
    }

    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        return Err(BinaryError::PodmanFailed(
            "podman create returned an empty container id".to_string(),
        ));
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "podman-create",
        format!("created container {id}"),
        None,
        log_path,
        Map::new(),
    );

    Ok(ContainerInstance { id })
}

fn export_container_output(
    instance: &ContainerInstance,
    output_path: &Path,
    cx: &BuildContext,
) -> BResult<()> {
    fsutil::recreate_empty_dir_force(output_path).map_err(map_fsutil_error)?;
    cx.log_event(
        BuildLogLevel::Info,
        "podman-cp",
        format!(
            "exporting build result from container {} to '{}'",
            instance.id,
            output_path.display()
        ),
    );
    let mut process = ProcessCommand::new("podman");
    process
        .arg("cp")
        .arg(format!("{}:{}/.", instance.id, OUT_DIR_MOUNT_PATH))
        .arg(output_path);
    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!("failed to execute podman cp: {error}"))
    })?;
    let log_path = write_command_log(cx, "podman-cp", "", &[], &output);

    if !output.status.success() {
        return Err(command_failure("podman-cp", "podman cp", &output, log_path));
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "podman-cp",
        format!("exported build result from container {}", instance.id),
        None,
        log_path,
        Map::new(),
    );

    verify_exported_output(output_path, cx)?;

    Ok(())
}

fn verify_exported_output(output_path: &Path, cx: &BuildContext) -> BResult<()> {
    let map_io_error = |error: std::io::Error| {
        BinaryError::FsFailed(format!(
            "failed to inspect exported build result '{}': {error}",
            output_path.display()
        ))
    };

    if !output_path.exists() {
        return Err(BinaryError::BuildFailed(format!(
            "exported build result is missing: '{}'",
            output_path.display()
        )));
    }
    if !output_path.is_dir() {
        return Err(BinaryError::BuildFailed(format!(
            "exported build result is not a directory: '{}'",
            output_path.display()
        )));
    }

    let mut entries = fs::read_dir(output_path)
        .map_err(map_io_error)?
        .map(|entry| entry.map_err(&map_io_error))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    let preview = entries
        .iter()
        .take(32)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    let listing = if preview.is_empty() {
        "<empty>\n".to_string()
    } else {
        let mut rendered = preview.join("\n");
        rendered.push('\n');
        rendered
    };
    let raw_log_path = cx.write_raw_log("export-verify", &listing);

    let mut details = Map::new();
    details.insert("entry_count".to_string(), Value::from(entries.len() as u64));
    details.insert(
        "entries_preview".to_string(),
        Value::Array(preview.into_iter().map(Value::String).collect()),
    );
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "export-verify",
        format!(
            "verified exported build result at '{}'",
            output_path.display()
        ),
        None,
        raw_log_path,
        details,
    );

    Ok(())
}

fn prepare_container_workspace(
    instance: &ContainerInstance,
    uid: u32,
    gid: u32,
    cx: &BuildContext,
) -> BResult<()> {
    cx.log_event(
        BuildLogLevel::Info,
        "podman-prepare",
        format!("preparing workspace in container {}", instance.id),
    );
    let mut process = ProcessCommand::new("podman");
    process
        .arg("exec")
        .arg("--user")
        .arg("0:0")
        .arg(&instance.id)
        .arg("sh")
        .arg("-lc")
        .arg(format!(
            "mkdir -p '{}' '{}' && chown {}:{} '{}'",
            BUILD_DIR_MOUNT_PATH, OUT_DIR_MOUNT_PATH, uid, gid, BUILD_DIR_MOUNT_PATH
        ));
    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!(
            "failed to execute podman exec for workspace prepare: {error}"
        ))
    })?;
    let log_path = write_command_log(cx, "podman-prepare", "", &[], &output);

    if !output.status.success() {
        return Err(command_failure(
            "podman-prepare",
            "podman exec prepare-workspace",
            &output,
            log_path,
        ));
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "podman-prepare",
        format!("prepared workspace in container {}", instance.id),
        None,
        log_path,
        Map::new(),
    );

    Ok(())
}

fn start_container(instance: &ContainerInstance, cx: &BuildContext) -> BResult<()> {
    cx.log_event(
        BuildLogLevel::Info,
        "podman-start",
        format!("starting build container {}", instance.id),
    );
    let mut process = ProcessCommand::new("podman");
    process.arg("start").arg(&instance.id);
    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!("failed to execute podman start: {error}"))
    })?;
    let log_path = write_command_log(cx, "podman-start", "", &[], &output);

    if !output.status.success() {
        return Err(command_failure(
            "podman-start",
            "podman start",
            &output,
            log_path,
        ));
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "podman-start",
        format!("started container {}", instance.id),
        None,
        log_path,
        Map::new(),
    );

    Ok(())
}

fn exec_step(
    instance: &ContainerInstance,
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
    cx: &BuildContext,
) -> BResult<()> {
    let log_tag = format!("step-{}", step.name);
    let cwd = resolve_step_cwd(step, inputs)?;
    let argv = resolve_step_argv(step, inputs)?;
    let env = resolve_step_env(step, inputs)?;
    cx.log_event(
        BuildLogLevel::Info,
        &log_tag,
        format!("running '{}' in container {}", step.name, instance.id),
    );
    let mut process = ProcessCommand::new("podman");
    process.arg("exec");
    if matches!(step.run_as, StepUser::Root) {
        process.arg("--user").arg("0:0");
    }
    process
        .arg("--workdir")
        .arg(&cwd)
        .arg("--env")
        .arg(format!("{STEP_NAME_ENV_VAR}={}", step.name));
    for (key, value) in env {
        process.arg("--env").arg(format!("{key}={value}"));
    }
    process.arg(&instance.id);
    for arg in &argv {
        process.arg(arg);
    }
    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!(
            "failed to execute podman exec for step '{}': {error}",
            step.name
        ))
    })?;
    let log_path = write_command_log(cx, &log_tag, "", inputs, &output);

    if !output.status.success() {
        return Err(command_failure(
            &log_tag,
            &format!("step '{}'", step.name),
            &output,
            log_path,
        ));
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        &log_tag,
        format!("step '{}' completed", step.name),
        None,
        log_path,
        Map::new(),
    );

    Ok(())
}

fn remove_container(instance: &ContainerInstance, cx: &BuildContext) -> Result<(), CleanupError> {
    cx.log_event(
        BuildLogLevel::Info,
        "podman-cleanup",
        format!("removing build container {}", instance.id),
    );
    let mut process = ProcessCommand::new("podman");
    process.arg("rm").arg("--force").arg(&instance.id);
    let output = process.output().map_err(|error| CleanupError {
        message: format!("failed to execute podman rm: {error}"),
        raw_log_path: None,
    })?;
    let log_path = write_command_log(cx, "podman-cleanup", "", &[], &output);

    if !output.status.success() {
        return Err(CleanupError {
            message: command_failure("podman-cleanup", "podman rm", &output, log_path.clone())
                .to_string(),
            raw_log_path: log_path,
        });
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "podman-cleanup",
        format!("removed container {}", instance.id),
        None,
        log_path,
        Map::new(),
    );

    Ok(())
}

fn log_cleanup_warning(
    cx: &BuildContext,
    container_id: &str,
    error: &CleanupError,
    output_preserved: bool,
) {
    let message = if output_preserved {
        format!(
            "failed to remove build container {container_id} after successful output export; build result was preserved: {}",
            error.message
        )
    } else {
        format!(
            "failed to remove build container {container_id} during cleanup: {}",
            error.message
        )
    };
    cx.log_event_with_details(
        BuildLogLevel::Warn,
        "cleanup-warning",
        message,
        None,
        error.raw_log_path.clone(),
        Map::new(),
    );
}

fn command_failure(
    log_tag: &str,
    command_name: &str,
    output: &std::process::Output,
    log_path: Option<PathBuf>,
) -> BinaryError {
    let log_hint = match &log_path {
        Some(path) => format!(" (log: {})", path.display()),
        None => String::new(),
    };
    let details = command_details(output);
    let exit_code = output.status.code().unwrap_or(1);
    match log_tag {
        tag if tag.starts_with("step-") => BinaryError::BuildFailed(format!(
            "{command_name} failed with exit status {exit_code}: {details}{log_hint}"
        )),
        _ => BinaryError::PodmanFailed(format!(
            "{command_name} failed with exit status {exit_code}: {details}{log_hint}"
        )),
    }
}

fn write_command_log(
    cx: &BuildContext,
    log_tag: &str,
    image_ref: &str,
    inputs: &[(String, BuilderInputObject)],
    output: &std::process::Output,
) -> Option<PathBuf> {
    let input_paths = if inputs.is_empty() {
        String::new()
    } else {
        let mut rendered = String::new();
        for (name, input) in inputs {
            rendered.push_str(&format!("{name}: {}\n", input.object_path.display()));
        }
        rendered
    };
    let log_content = format!(
        "image_ref: {}\ninputs:\n{}exit_code: {}\nstatus_success: {}\n\n=== stdout ===\n{}\n\n=== stderr ===\n{}\n",
        image_ref,
        input_paths,
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string()),
        output.status.success(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    cx.write_raw_log(log_tag, &log_content)
}

fn command_details(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "command failed without output".to_string(),
        (false, true) => format!("stdout: {stdout}"),
        (true, false) => format!("stderr: {stderr}"),
        (false, false) => format!("stdout: {stdout}; stderr: {stderr}"),
    }
}

fn map_fsutil_error(error: fsutil::FsUtilError) -> BinaryError {
    BinaryError::FsFailed(error.to_string())
}

fn map_error(error: BinaryError) -> BuilderError {
    match error {
        BinaryError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        BinaryError::InputResolutionFailed(message)
        | BinaryError::PodmanFailed(message)
        | BinaryError::BuildFailed(message)
        | BinaryError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}

fn validate_script_config(value: Option<&Value>) -> BResult<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => validate_script_config_node(value, "<root>"),
    }
}

fn validate_script_config_node(value: &Value, path: &str) -> BResult<()> {
    match value {
        Value::String(_) => Ok(()),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                validate_script_config_node(item, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (key, item) in map {
                validate_script_config_key(key, path)?;
                validate_script_config_node(item, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        _ => Err(BinaryError::InvalidConfig(format!(
            "script_config supports only records, arrays, and string leaves; invalid value at {path}"
        ))),
    }
}

fn validate_script_config_key(key: &str, path: &str) -> BResult<()> {
    if key.is_empty()
        || key == "."
        || key == ".."
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(BinaryError::InvalidConfig(format!(
            "script_config key '{}' at {} is invalid; allowed chars: [A-Za-z0-9._-]",
            key, path
        )));
    }
    Ok(())
}

fn write_script_config(root: &Path, value: Option<&Value>) -> BResult<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => write_script_config_node(root, value, "<root>"),
    }
}

fn write_script_config_node(path: &Path, value: &Value, debug_path: &str) -> BResult<()> {
    match value {
        Value::String(contents) => fs::write(path, contents).map_err(|error| {
            BinaryError::FsFailed(format!(
                "failed to write script_config leaf '{}' to '{}': {error}",
                debug_path,
                path.display()
            ))
        }),
        Value::Array(items) => {
            fsutil::recreate_empty_dir(path).map_err(map_fsutil_error)?;
            for (index, item) in items.iter().enumerate() {
                let child_path = path.join(format!("{index:08}"));
                write_script_config_node(&child_path, item, &format!("{debug_path}[{index}]"))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            fsutil::recreate_empty_dir(path).map_err(map_fsutil_error)?;
            for (key, item) in map {
                let child_path = path.join(key);
                write_script_config_node(&child_path, item, &format!("{debug_path}.{key}"))?;
            }
            Ok(())
        }
        _ => Err(BinaryError::InvalidConfig(format!(
            "script_config supports only records, arrays, and string leaves; invalid value at {debug_path}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{BuildLogEvent, BuildLogger, Builder, BuilderInputObject, BuilderInputs};
    use std::env;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex, OnceLock};
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn build_context(root: &Path) -> BuildContext {
        let state_dir = root.join(".mbuild").join("builder-state").join("binary");
        let temp_dir = state_dir.join("tmp");
        fs::create_dir_all(&state_dir).unwrap();
        mbuild_core::fsutil::recreate_empty_dir_force(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    #[derive(Debug)]
    struct CapturingLogger {
        root: PathBuf,
        events: Mutex<Vec<BuildLogEvent>>,
    }

    impl CapturingLogger {
        fn new(root: PathBuf) -> Self {
            fs::create_dir_all(&root).unwrap();
            Self {
                root,
                events: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<BuildLogEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl BuildLogger for CapturingLogger {
        fn log_event(&self, event: BuildLogEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, String> {
            let mut index = self.events.lock().unwrap().len();
            loop {
                let path = self.root.join(format!("{label}-{index}.log"));
                if !path.exists() {
                    return Ok(path);
                }
                index += 1;
            }
        }
    }

    fn install_fake_podman(dir: &Path) {
        let script_path = dir.join("podman");
        fs::write(
            &script_path,
            include_str!("../tests/assets/fake_podman_run.sh"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
        }
    }

    fn with_fake_podman<T>(f: impl FnOnce() -> T) -> T {
        let _guard = env_lock().lock().unwrap();
        let temp = tempdir().unwrap();
        install_fake_podman(temp.path());
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

    fn resolved_directory(
        root: &Path,
        name: &str,
        extra_meta: Map<String, Value>,
    ) -> BuilderInputObject {
        let object_path = root.join(name);
        fs::create_dir_all(&object_path).unwrap();
        fs::write(object_path.join("README.txt"), b"hello source\n").unwrap();
        let meta = extra_meta;
        BuilderInputObject { object_path, meta }
    }

    fn resolved_file(
        root: &Path,
        name: &str,
        executable: bool,
        extra_meta: Map<String, Value>,
    ) -> BuilderInputObject {
        let object_path = root.join(name);
        fs::write(&object_path, b"payload\n").unwrap();
        #[cfg(unix)]
        if executable {
            let mut permissions = fs::metadata(&object_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&object_path, permissions).unwrap();
        }
        let meta = extra_meta;
        BuilderInputObject { object_path, meta }
    }

    fn resolved_oci_layout(
        root: &Path,
        name: &str,
        extra_meta: Map<String, Value>,
    ) -> BuilderInputObject {
        let object_path = root.join(name);
        create_test_oci_layout_at(&object_path);
        let meta = extra_meta;
        BuilderInputObject { object_path, meta }
    }

    /// Create a minimal valid OCI layout directory at the given path.
    fn create_test_oci_layout_at(oci_dir: &Path) {
        fs::create_dir_all(oci_dir.join("blobs").join("sha256")).unwrap();
        fs::write(
            oci_dir.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let config_bytes = b"{}";
        let config_digest_hex = sha256_hex_test(config_bytes);
        let config_digest = format!("sha256:{config_digest_hex}");
        fs::write(
            oci_dir
                .join("blobs")
                .join("sha256")
                .join(&config_digest_hex),
            config_bytes,
        )
        .unwrap();

        let layer_bytes: &[u8] =
            b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let layer_digest_hex = sha256_hex_test(layer_bytes);
        let layer_digest = format!("sha256:{layer_digest_hex}");
        fs::write(
            oci_dir.join("blobs").join("sha256").join(&layer_digest_hex),
            layer_bytes,
        )
        .unwrap();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config_bytes.len()},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer_digest, "size": layer_bytes.len()}]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest_hex = sha256_hex_test(&manifest_bytes);
        let manifest_digest = format!("sha256:{manifest_digest_hex}");
        fs::write(
            oci_dir
                .join("blobs")
                .join("sha256")
                .join(&manifest_digest_hex),
            &manifest_bytes,
        )
        .unwrap();

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": manifest_digest, "size": manifest_bytes.len()}]
        });
        fs::write(
            oci_dir.join("index.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
    }

    fn sha256_hex_test(data: &[u8]) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        // We don't have sha2 as a dep here; use a deterministic pseudo-digest
        // by hashing the data and padding to 64 hex chars. Good enough for
        // test fixtures that just need unique, stable identifiers.
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let h = hasher.finish();
        let padded = data.len();
        format!("{h:016x}{h:016x}{h:016x}{padded:016x}")
    }

    fn sample_inputs(root: &Path) -> BuilderInputs {
        let mut inputs = BuilderInputs::empty();
        inputs.insert("image", resolved_oci_layout(root, "image-oci", Map::new()));
        inputs.insert("script", resolved_file(root, "script.sh", true, Map::new()));
        inputs.insert("source", resolved_directory(root, "src", Map::new()));
        inputs
    }

    fn sample_inputs_with_aux_file(root: &Path) -> BuilderInputs {
        let mut inputs = sample_inputs(root);
        inputs.insert(
            "patch",
            resolved_file(root, "patch.diff", false, Map::new()),
        );
        inputs
    }

    fn sample_inputs_with_binary_output_aux(root: &Path) -> BuilderInputs {
        let mut inputs = sample_inputs(root);
        inputs.insert(
            "linux_headers",
            resolved_directory(root, "linux-headers", Map::new()),
        );
        inputs
    }

    fn default_steps() -> Vec<BuildStep> {
        vec![
            BuildStep {
                name: "configure".to_string(),
                run_as: StepUser::BuildUser,
                cwd: "@{build}".to_string(),
                argv: vec!["@{script}".to_string(), "configure".to_string()],
                env: Map::new(),
            },
            BuildStep {
                name: "build".to_string(),
                run_as: StepUser::BuildUser,
                cwd: "@{build}".to_string(),
                argv: vec!["@{script}".to_string(), "build".to_string()],
                env: Map::new(),
            },
            BuildStep {
                name: "install".to_string(),
                run_as: StepUser::Root,
                cwd: "@{build}".to_string(),
                argv: vec!["@{script}".to_string(), "install".to_string()],
                env: Map::new(),
            },
            BuildStep {
                name: "post_install".to_string(),
                run_as: StepUser::Root,
                cwd: "@{build}".to_string(),
                argv: vec!["@{script}".to_string(), "post_install".to_string()],
                env: Map::new(),
            },
        ]
    }

    fn default_config() -> BinaryConfig {
        BinaryConfig {
            script_config: None,
            steps: default_steps(),
            install: None,
        }
    }

    fn source_free_config() -> BinaryConfig {
        BinaryConfig {
            script_config: None,
            steps: vec![
                BuildStep {
                    name: "configure".to_string(),
                    run_as: StepUser::BuildUser,
                    cwd: "@{build}".to_string(),
                    argv: vec!["@{script}".to_string(), "configure".to_string()],
                    env: Map::new(),
                },
                BuildStep {
                    name: "build".to_string(),
                    run_as: StepUser::BuildUser,
                    cwd: "@{build}".to_string(),
                    argv: vec!["@{script}".to_string(), "build".to_string()],
                    env: Map::new(),
                },
                BuildStep {
                    name: "install".to_string(),
                    run_as: StepUser::Root,
                    cwd: "@{build}".to_string(),
                    argv: vec!["@{script}".to_string(), "install".to_string()],
                    env: Map::new(),
                },
                BuildStep {
                    name: "post_install".to_string(),
                    run_as: StepUser::Root,
                    cwd: "@{build}".to_string(),
                    argv: vec!["@{script}".to_string(), "post_install".to_string()],
                    env: Map::new(),
                },
            ],
            install: None,
        }
    }

    #[test]
    fn binary_builder_runs_fake_podman_and_materializes_output_dir() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap();
            assert!(result.meta.get("install").is_some());
            assert!(result.staged_path.is_dir());
            assert_eq!(
                fs::read_to_string(result.staged_path.join("copied").join("README.txt")).unwrap(),
                "hello source\n"
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("script-mount.txt")).unwrap(),
                format!("{}/script\n", INPUT_MOUNT_ROOT)
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("source-mount.txt")).unwrap(),
                format!("{}/source\n", INPUT_MOUNT_ROOT)
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("build-dir.txt")).unwrap(),
                format!("{BUILD_DIR_MOUNT_PATH}\n")
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("out-dir.txt")).unwrap(),
                format!("{OUT_DIR_MOUNT_PATH}\n")
            );
            let image_ref = fs::read_to_string(result.staged_path.join("image-ref.txt")).unwrap();
            let image_ref = image_ref.trim();
            assert!(
                image_ref.starts_with("sha256:") && image_ref.len() == 71,
                "expected sha256:<64hex> image ref, got: {image_ref}"
            );
        });
    }

    #[test]
    fn binary_builder_materializes_script_config_dir() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(
                    BinaryConfig {
                        script_config: Some(serde_json::json!({
                            "configure_args": ["--disable-nls", "--without-selinux"],
                            "env": {
                                "CC": "gcc",
                                "CFLAGS": "-O2",
                            },
                            "pre_configure": "echo pre",
                        })),
                        steps: default_steps(),
                        install: None,
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
                .unwrap();

            assert_eq!(
                fs::read_to_string(result.staged_path.join("script-config-dir.txt")).unwrap(),
                format!("{CONFIG_MOUNT_PATH}\n")
            );
            assert_eq!(
                fs::read_to_string(
                    result
                        .staged_path
                        .join("script-config")
                        .join("configure_args")
                        .join("00000000")
                )
                .unwrap(),
                "--disable-nls"
            );
            assert_eq!(
                fs::read_to_string(
                    result
                        .staged_path
                        .join("script-config")
                        .join("configure_args")
                        .join("00000001")
                )
                .unwrap(),
                "--without-selinux"
            );
            assert_eq!(
                fs::read_to_string(
                    result
                        .staged_path
                        .join("script-config")
                        .join("env")
                        .join("CC")
                )
                .unwrap(),
                "gcc"
            );
            assert_eq!(
                fs::read_to_string(
                    result
                        .staged_path
                        .join("script-config")
                        .join("pre_configure")
                )
                .unwrap(),
                "echo pre"
            );
        });
    }

    #[test]
    fn binary_builder_accepts_fetched_file_as_auxiliary_source() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(
                    default_config(),
                    sample_inputs_with_aux_file(temp.path()),
                    &mut cx,
                )
                .unwrap();
            assert!(result.meta.get("install").is_some());
            assert!(result.staged_path.is_dir());
        });
    }

    #[test]
    fn binary_builder_accepts_zero_sources_for_source_free_artifacts() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let builder = BinaryBuilder;

            let image = resolved_oci_layout(temp.path(), "image-oci-zero", Map::new());
            let script = resolved_file(temp.path(), "script.sh", true, Map::new());

            let inputs = BuilderInputs::new(std::collections::BTreeMap::from([
                ("image".to_string(), image.clone()),
                ("script".to_string(), script.clone()),
            ]));

            let result = builder
                .build_typed(source_free_config(), inputs, &mut cx)
                .unwrap();
            assert!(result.meta.get("install").is_some());
            assert!(result.staged_path.is_dir());
        });
    }

    #[test]
    fn binary_builder_accepts_binary_output_as_auxiliary_source() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(
                    default_config(),
                    sample_inputs_with_binary_output_aux(temp.path()),
                    &mut cx,
                )
                .unwrap();
            assert!(result.meta.get("install").is_some());
            assert!(result.staged_path.is_dir());
        });
    }

    #[test]
    fn binary_builder_rejects_non_oci_layout_image_input() {
        // A plain directory without oci-layout marker should be rejected.
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let bad_image_dir = temp.path().join("not-an-oci-dir");
        fs::create_dir(&bad_image_dir).unwrap();
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "image",
            BuilderInputObject {
                object_path: bad_image_dir,
                meta: Map::new(),
            },
        );
        inputs.insert(
            "script",
            resolved_file(temp.path(), "script.sh", true, Map::new()),
        );
        inputs.insert("source", resolved_directory(temp.path(), "src", Map::new()));

        let error = BinaryBuilder
            .build_typed(default_config(), inputs, &mut cx)
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn binary_builder_uses_config_digest_as_image_ref() {
        // The image ref passed to podman create should be the config blob digest
        // (sha256:<config_hex>) extracted from the OCI layout.
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap();

            let image_ref = fs::read_to_string(result.staged_path.join("image-ref.txt")).unwrap();
            let image_ref = image_ref.trim();
            assert!(
                image_ref.starts_with("sha256:") && image_ref.len() == 71,
                "expected sha256:<64hex> from OCI config digest, got: {image_ref}"
            );
        });
    }

    #[test]
    fn binary_builder_runs_phases_with_split_user_contexts() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap();

            assert_eq!(
                fs::read_to_string(result.staged_path.join("userns-mode.txt")).unwrap(),
                "keep-id\n"
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("install-user.txt")).unwrap(),
                "0:0\n"
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("post-install-user.txt")).unwrap(),
                "0:0\n"
            );
            assert_eq!(
                fs::read_to_string(result.staged_path.join("phases.txt")).unwrap(),
                "configure\nbuild\ninstall\npost_install\n"
            );
            assert_ne!(
                fs::read_to_string(result.staged_path.join("configure-user.txt")).unwrap(),
                "0:0\n"
            );
            assert_ne!(
                fs::read_to_string(result.staged_path.join("build-user.txt")).unwrap(),
                "0:0\n"
            );
        });
    }

    #[test]
    fn binary_builder_preserves_explicit_install_rules() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(
                    BinaryConfig {
                        script_config: None,
                        steps: default_steps(),
                        install: Some(InstallMeta {
                            rules: vec![InstallRule {
                                path: "etc/shadow".to_string(),
                                attrs: InstallAttrs {
                                    uid: Some(0),
                                    gid: Some(42),
                                    directory_mode: None,
                                    regular_file_mode: Some(0o640),
                                    executable_file_mode: None,
                                    symlink_mode: None,
                                },
                            }],
                        }),
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
                .unwrap();

            assert_eq!(
                result.meta.get("install").unwrap(),
                &serde_json::to_value(InstallMeta {
                    rules: vec![InstallRule {
                        path: "etc/shadow".to_string(),
                        attrs: InstallAttrs {
                            uid: Some(0),
                            gid: Some(42),
                            directory_mode: None,
                            regular_file_mode: Some(0o640),
                            executable_file_mode: None,
                            symlink_mode: None,
                        },
                    }],
                })
                .unwrap()
            );
        });
    }

    #[test]
    fn binary_builder_rejects_non_string_script_config_leaves() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let error = BinaryBuilder
            .build_typed(
                BinaryConfig {
                    script_config: Some(serde_json::json!({
                        "configure_args": ["--disable-nls"],
                        "jobs": 4,
                    })),
                    steps: default_steps(),
                    install: None,
                },
                sample_inputs(temp.path()),
                &mut cx,
            )
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("script_config"), "{message}");
        assert!(message.contains("string leaves"), "{message}");
    }

    #[test]
    fn binary_builder_rejects_invalid_script_config_keys() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let error = BinaryBuilder
            .build_typed(
                BinaryConfig {
                    script_config: Some(serde_json::json!({
                        "bad key": "value",
                    })),
                    steps: default_steps(),
                    install: None,
                },
                sample_inputs(temp.path()),
                &mut cx,
            )
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("script_config key"), "{message}");
        assert!(message.contains("bad key"), "{message}");
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = BinaryBuilder
            .build_erased(
                serde_json::json!({
                    "extra": true,
                }),
                sample_inputs(temp.path()),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn binary_builder_reports_step_failure() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            unsafe { env::set_var("MBUILD_TEST_BINARY_PODMAN_FAIL", "1") };
            let error = BinaryBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap_err();
            unsafe { env::remove_var("MBUILD_TEST_BINARY_PODMAN_FAIL") };

            assert!(matches!(error, BuilderError::ExecutionFailed(_)));
            let message = error.to_string();
            assert!(message.contains("step 'configure'"), "{message}");
        });
    }

    #[test]
    fn binary_builder_rejects_missing_exported_output_after_podman_cp() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            unsafe { env::set_var("MBUILD_TEST_BINARY_PODMAN_CP_REMOVE_DEST", "1") };
            let error = BinaryBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap_err();
            unsafe { env::remove_var("MBUILD_TEST_BINARY_PODMAN_CP_REMOVE_DEST") };

            assert!(matches!(error, BuilderError::ExecutionFailed(_)));
            let message = error.to_string();
            assert!(
                message.contains("exported build result is missing"),
                "{message}"
            );
        });
    }

    #[test]
    fn binary_builder_warns_when_cleanup_fails_after_successful_export() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let logger = Arc::new(CapturingLogger::new(temp.path().join("logs")));
            let mut cx = build_context(temp.path()).with_logger(logger.clone());
            unsafe { env::set_var("MBUILD_TEST_BINARY_PODMAN_RM_FAIL", "1") };
            let result = BinaryBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap();
            unsafe { env::remove_var("MBUILD_TEST_BINARY_PODMAN_RM_FAIL") };

            assert!(result.staged_path.is_dir());
            let events = logger.events();
            let warning = events
                .iter()
                .find(|event| {
                    event.level == BuildLogLevel::Warn && event.phase == "cleanup-warning"
                })
                .expect("expected cleanup warning event");
            assert!(
                warning.message.contains("successful output export"),
                "{}",
                warning.message
            );
            assert!(
                warning.message.contains("build result was preserved"),
                "{}",
                warning.message
            );
            assert!(warning.raw_log_path.is_some());
        });
    }

    #[test]
    fn binary_builder_preserves_step_failure_when_cleanup_also_fails() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let logger = Arc::new(CapturingLogger::new(temp.path().join("logs")));
            let mut cx = build_context(temp.path()).with_logger(logger.clone());
            unsafe {
                env::set_var("MBUILD_TEST_BINARY_PODMAN_FAIL", "1");
                env::set_var("MBUILD_TEST_BINARY_PODMAN_RM_FAIL", "1");
            }
            let error = BinaryBuilder
                .build_typed(default_config(), sample_inputs(temp.path()), &mut cx)
                .unwrap_err();
            unsafe {
                env::remove_var("MBUILD_TEST_BINARY_PODMAN_FAIL");
                env::remove_var("MBUILD_TEST_BINARY_PODMAN_RM_FAIL");
            }

            let message = error.to_string();
            assert!(message.contains("step 'configure'"), "{message}");
            assert!(!message.contains("podman rm"), "{message}");

            let events = logger.events();
            let warning = events
                .iter()
                .find(|event| {
                    event.level == BuildLogLevel::Warn && event.phase == "cleanup-warning"
                })
                .expect("expected cleanup warning event");
            assert!(
                warning.message.contains("during cleanup"),
                "{}",
                warning.message
            );
            assert!(warning.raw_log_path.is_some());
        });
    }

    #[test]
    fn binary_builder_treats_legacy_interpolation_syntax_as_literal_text() {
        let temp = tempdir().unwrap();
        let step = BuildStep {
            name: "install".to_string(),
            run_as: StepUser::Root,
            cwd: "@{build}".to_string(),
            argv: vec!["${in}".to_string(), "${source}".to_string()],
            env: Map::from_iter([("LITERAL".to_string(), Value::String("${out}".to_string()))]),
        };

        let argv = resolve_step_argv(
            &step,
            &[(
                "source".to_string(),
                resolved_directory(temp.path(), "source", Map::new()),
            )],
        )
        .unwrap();
        let env = resolve_step_env(
            &step,
            &[(
                "source".to_string(),
                resolved_directory(temp.path(), "source", Map::new()),
            )],
        )
        .unwrap();

        assert_eq!(argv, vec!["${in}".to_string(), "${source}".to_string()]);
        assert_eq!(env, vec![("LITERAL".to_string(), "${out}".to_string())]);
    }

    #[test]
    fn binary_builder_rejects_missing_input_interpolation() {
        let temp = tempdir().unwrap();
        let step = BuildStep {
            name: "install".to_string(),
            run_as: StepUser::Root,
            cwd: "@{build}".to_string(),
            argv: vec!["@{missing_patch}".to_string(), "install".to_string()],
            env: Map::new(),
        };
        let error = resolve_step_argv(
            &step,
            &[(
                "script".to_string(),
                resolved_file(temp.path(), "script.sh", true, Map::new()),
            )],
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("unknown interpolation variable"),
            "{message}"
        );
    }

    #[test]
    fn binary_builder_resolves_interpolation_escape_as_literal() {
        let temp = tempdir().unwrap();
        let step = BuildStep {
            name: "install".to_string(),
            run_as: StepUser::Root,
            cwd: "@{build}".to_string(),
            argv: vec![
                "@@{patch}".to_string(),
                "cp".to_string(),
                "@{source}/x".to_string(),
                "$HOME/@@{literal}".to_string(),
            ],
            env: Map::new(),
        };
        let argv = resolve_step_argv(
            &step,
            &[(
                "source".to_string(),
                resolved_directory(temp.path(), "source", Map::new()),
            )],
        )
        .unwrap();

        assert_eq!(argv[0], "@{patch}");
        assert_eq!(argv[1], "cp");
        assert_eq!(argv[2], format!("{}/x", input_mount_path("source")));
        assert_eq!(argv[3], "$HOME/@{literal}");
    }

    #[test]
    fn binary_builder_rejects_unterminated_new_interpolation() {
        let step = BuildStep {
            name: "install".to_string(),
            run_as: StepUser::Root,
            cwd: "@{build".to_string(),
            argv: vec!["@{missing".to_string(), "install".to_string()],
            env: Map::new(),
        };
        let error = resolve_step_argv(&step, &[]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("unterminated interpolation"), "{message}");
    }

    #[test]
    fn binary_builder_rejects_unterminated_interpolation_escape() {
        let step = BuildStep {
            name: "install".to_string(),
            run_as: StepUser::Root,
            cwd: "@{build}".to_string(),
            argv: vec!["@@{literal".to_string(), "install".to_string()],
            env: Map::new(),
        };
        let error = resolve_step_argv(&step, &[]).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("unterminated interpolation escape"),
            "{message}"
        );
    }

    #[test]
    fn binary_builder_rejects_invalid_new_interpolation_name() {
        let step = BuildStep {
            name: "install".to_string(),
            run_as: StepUser::Root,
            cwd: "@{build}".to_string(),
            argv: vec!["@{foo-bar}".to_string(), "install".to_string()],
            env: Map::new(),
        };
        let error = resolve_step_argv(&step, &[]).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("invalid interpolation variable"),
            "{message}"
        );
    }
}
