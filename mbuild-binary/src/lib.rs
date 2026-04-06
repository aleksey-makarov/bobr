use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    InputArity, InputSlot, StagedBuildResult, TypedBuilder, fsutil,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

const OUTPUT_DIR_NAME: &str = "out";
const SCRIPT_CONFIG_DIR_NAME: &str = "script-config";
const SCRIPT_CONFIG_MOUNT_PATH: &str = "/__mbuild_script_config";
const SCRIPT_CONFIG_ENV_VAR: &str = "MBUILD_SCRIPT_CONFIG_DIR";

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryConfig {
    #[serde(default)]
    script_config: Option<Value>,
}

struct ScriptExecution {
    script_host_path: PathBuf,
    config_host_path: PathBuf,
    source_input_name: String,
}

struct ContainerExecution {
    image_ref: String,
}

pub struct BinaryBuilder;

static BINARY_INPUTS: &[InputSlot] = &[
    InputSlot {
        name: "image",
        arity: InputArity::One,
    },
    InputSlot {
        name: "script",
        arity: InputArity::One,
    },
    InputSlot {
        name: "sources",
        arity: InputArity::Many,
    },
];

static BINARY_SPEC: BuilderSpec = BuilderSpec {
    tag: "Binary",
    inputs: BINARY_INPUTS,
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
        let image = inputs.one("image")?;
        let script = inputs.one("script")?;
        let sources = inputs.many("sources")?;

        let output_path = cx.temp_dir.join(OUTPUT_DIR_NAME);
        fsutil::recreate_empty_dir(&output_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let config_path = cx.temp_dir.join(SCRIPT_CONFIG_DIR_NAME);
        fsutil::recreate_empty_dir(&config_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        write_script_config(&config_path, config.script_config.as_ref()).map_err(map_error)?;

        let script_execution =
            resolve_script_execution(script, &config_path, sources).map_err(map_error)?;
        let container_execution = resolve_container_execution(image, cx).map_err(map_error)?;
        cx.log_event(
            BuildLogLevel::Info,
            "prepare",
            format!(
                "resolved container image, build script, {} source input(s), and script config dir",
                sources.len()
            ),
        );

        let build_result = run_container_build(
            &container_execution,
            &script_execution,
            sources,
            &output_path,
            current_uid_gid(),
            cx,
        );

        if let Err(error) = build_result {
            return Err(map_error(error));
        }

        let mut meta = Map::new();
        meta.insert(
            "install".to_string(),
            serde_json::json!({
                "owners": [
                    {
                        "path": "**",
                        "uid": 0,
                        "gid": 0,
                    }
                ]
            }),
        );

        Ok(StagedBuildResult {
            meta,
            staged_path: output_path,
        })
    }
}

fn validate_config(config: &BinaryConfig) -> BResult<()> {
    validate_script_config(config.script_config.as_ref())?;
    Ok(())
}

fn resolve_script_execution(
    script: &BuilderInputObject,
    script_config_dir: &Path,
    sources: &[BuilderInputObject],
) -> BResult<ScriptExecution> {
    if !script.object_path.is_file() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "script input must resolve to a file: {}",
            script.object_path.display()
        )));
    }
    if let Some((first, rest)) = sources.split_first() {
        if !first.object_path.is_dir() {
            return Err(BinaryError::InputResolutionFailed(format!(
                "first source input must resolve to a directory: {}",
                first.object_path.display()
            )));
        }
        for source in rest {
            if !source.object_path.is_dir() && !source.object_path.is_file() {
                return Err(BinaryError::InputResolutionFailed(format!(
                    "additional source inputs must resolve to directories or files: {}",
                    source.object_path.display()
                )));
            }
        }
    }

    Ok(ScriptExecution {
        script_host_path: script.object_path.clone(),
        config_host_path: script_config_dir.to_path_buf(),
        source_input_name: if sources.is_empty() {
            String::new()
        } else {
            "sources0".to_string()
        },
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
    script: &ScriptExecution,
    sources: &[BuilderInputObject],
    output_path: &Path,
    (uid, gid): (u32, u32),
    cx: &BuildContext,
) -> BResult<()> {
    cx.log_event(
        BuildLogLevel::Info,
        "podman-run",
        format!("running podman with image '{}'", container.image_ref),
    );
    let mut process = ProcessCommand::new("podman");
    process
        .arg("run")
        .arg("--rm")
        .arg("--network=none")
        .arg("--userns=keep-id")
        .arg("--user")
        .arg(format!("{}:{}", uid, gid));

    for (index, source) in sources.iter().enumerate() {
        let mount_spec = if source.object_path.is_dir() {
            format!("{}:/in/sources{}:O", source.object_path.display(), index)
        } else {
            format!("{}:/in/sources{}:ro", source.object_path.display(), index)
        };
        process.arg("--volume").arg(mount_spec);
    }

    process.arg("--volume").arg(format!(
        "{}:/out/{}:rw",
        output_path.display(),
        OUTPUT_DIR_NAME
    ));

    process.arg("--volume").arg(format!(
        "{}:/__mbuild_binary_script:ro",
        script.script_host_path.display()
    ));
    process.arg("--volume").arg(format!(
        "{}:{}:ro",
        script.config_host_path.display(),
        SCRIPT_CONFIG_MOUNT_PATH
    ));

    process
        .arg("--env")
        .arg(format!("MBUILD_SOURCE_INPUT={}", script.source_input_name))
        .arg("--env")
        .arg(format!("MBUILD_PRIMARY_OUTPUT={OUTPUT_DIR_NAME}"))
        .arg("--env")
        .arg(format!(
            "{SCRIPT_CONFIG_ENV_VAR}={SCRIPT_CONFIG_MOUNT_PATH}"
        ))
        .arg(&container.image_ref)
        .arg("/__mbuild_binary_script");

    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!("failed to execute podman run: {error}"))
    })?;
    let log_path = write_run_log(
        cx,
        &container.image_ref,
        &script.script_host_path,
        &script.source_input_name,
        &output,
    );

    if !output.status.success() {
        cx.log_event_with_details(
            BuildLogLevel::Error,
            "command-fail",
            format!("podman run failed: {}", command_details(&output)),
            None,
            log_path.clone(),
            Map::new(),
        );
        let log_hint = match &log_path {
            Some(path) => format!(" (log: {})", path.display()),
            None => String::new(),
        };
        return Err(BinaryError::BuildFailed(format!(
            "podman run failed with exit status {}: {}{}",
            output.status.code().unwrap_or(1),
            command_details(&output),
            log_hint,
        )));
    }

    if !output_path.is_dir() {
        return Err(BinaryError::BuildFailed(format!(
            "binary builder did not produce output directory '{}'",
            output_path.display()
        )));
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "podman-run",
        "podman run completed",
        None,
        log_path,
        Map::new(),
    );

    Ok(())
}

fn write_run_log(
    cx: &BuildContext,
    image_ref: &str,
    script_path: &Path,
    source_input_name: &str,
    output: &std::process::Output,
) -> Option<PathBuf> {
    let log_content = format!(
        "image_ref: {}\nscript: {}\nsource_input: {}\nexit_code: {}\nstatus_success: {}\n\n=== stdout ===\n{}\n\n=== stderr ===\n{}\n",
        image_ref,
        script_path.display(),
        source_input_name,
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string()),
        output.status.success(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    cx.write_raw_log("podman-run", &log_content)
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

fn current_uid_gid() -> (u32, u32) {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        (uid, gid)
    }

    #[cfg(not(unix))]
    {
        (0, 0)
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
    use mbuild_core::{Builder, BuilderInputObject, BuilderInputValue, BuilderInputs};
    use std::env;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};
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
        inputs.insert(
            "image",
            BuilderInputValue::One(resolved_oci_layout(root, "image-oci", Map::new())),
        );
        inputs.insert(
            "script",
            BuilderInputValue::One(resolved_file(root, "script.sh", true, Map::new())),
        );
        inputs.insert(
            "sources",
            BuilderInputValue::Many(vec![resolved_directory(root, "src", Map::new())]),
        );
        inputs
    }

    fn sample_inputs_with_aux_file(root: &Path) -> BuilderInputs {
        let mut inputs = sample_inputs(root);
        let mut sources = match inputs.many("sources").unwrap().to_vec() {
            values => values,
        };
        sources.push(resolved_file(root, "patch.diff", false, Map::new()));
        inputs.insert("sources", BuilderInputValue::Many(sources));
        inputs
    }

    fn sample_inputs_with_binary_output_aux(root: &Path) -> BuilderInputs {
        let mut inputs = sample_inputs(root);
        let mut sources = match inputs.many("sources").unwrap().to_vec() {
            values => values,
        };
        sources.push(resolved_directory(root, "linux-headers", Map::new()));
        inputs.insert("sources", BuilderInputValue::Many(sources));
        inputs
    }

    #[test]
    fn binary_builder_runs_fake_podman_and_materializes_output_dir() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(
                    BinaryConfig {
                        script_config: None,
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
                .unwrap();
            assert!(result.meta.get("install").is_some());
            assert!(result.staged_path.is_dir());
            assert_eq!(
                fs::read_to_string(result.staged_path.join("copied").join("README.txt")).unwrap(),
                "hello source\n"
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
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
                .unwrap();

            assert_eq!(
                fs::read_to_string(result.staged_path.join("script-config-dir.txt")).unwrap(),
                format!("{SCRIPT_CONFIG_MOUNT_PATH}\n")
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
                    BinaryConfig {
                        script_config: None,
                    },
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
                ("image".to_string(), BuilderInputValue::One(image.clone())),
                ("script".to_string(), BuilderInputValue::One(script.clone())),
                ("sources".to_string(), BuilderInputValue::Many(vec![])),
            ]));

            let result = builder
                .build_typed(
                    BinaryConfig {
                        script_config: None,
                    },
                    inputs,
                    &mut cx,
                )
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
                    BinaryConfig {
                        script_config: None,
                    },
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
            BuilderInputValue::One(BuilderInputObject {
                object_path: bad_image_dir,
                meta: Map::new(),
            }),
        );
        inputs.insert(
            "script",
            BuilderInputValue::One(resolved_file(temp.path(), "script.sh", true, Map::new())),
        );
        inputs.insert(
            "sources",
            BuilderInputValue::Many(vec![resolved_directory(temp.path(), "src", Map::new())]),
        );

        let error = BinaryBuilder
            .build_typed(
                BinaryConfig {
                    script_config: None,
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn binary_builder_uses_config_digest_as_image_ref() {
        // The image ref passed to podman run should be the config blob digest
        // (sha256:<config_hex>) extracted from the OCI layout.
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            let result = BinaryBuilder
                .build_typed(
                    BinaryConfig {
                        script_config: None,
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
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
    fn binary_builder_reports_podman_run_failure() {
        with_fake_podman(|| {
            let temp = tempdir().unwrap();
            let mut cx = build_context(temp.path());
            unsafe { env::set_var("MBUILD_TEST_BINARY_PODMAN_FAIL", "1") };
            let error = BinaryBuilder
                .build_typed(
                    BinaryConfig {
                        script_config: None,
                    },
                    sample_inputs(temp.path()),
                    &mut cx,
                )
                .unwrap_err();
            unsafe { env::remove_var("MBUILD_TEST_BINARY_PODMAN_FAIL") };

            assert!(matches!(error, BuilderError::ExecutionFailed(_)));
            let message = error.to_string();
            assert!(message.contains("podman run"), "{message}");
        });
    }
}
