//! Sandbox builder backed by `bobr-runtime`.
//!
//! It provides the `SandboxInstall` builder that executes
//! `bobr-sandbox-launcher` through a `bobr-runtime` function and publishes
//! fs-tree manifests.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod lifecycle;
mod mounts;
mod reports;
mod tools;

use bobr_builder::{BuildContext, Builder, BuilderError, BuilderInputs, InputSpec, TypedBuilder};
use bobr_core::BuildLogLevel;
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_sandbox_launcher::{
    CONTAINER_BUILD_DIR, CONTAINER_CONFIG_DIR, CONTAINER_INPUTS_DIR, CONTAINER_OUT_DIR,
    SandboxStepReport,
};
use bobr_store::fs_tree::FsTree;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Default file creation mask for sandbox steps.
const DEFAULT_SANDBOX_UMASK: u32 = 0o022;
const OUTPUT_DIR_NAME: &str = "out";
const CONFIG_DIR_NAME: &str = "config";
const RUNTIME_DIR_NAME: &str = "runtime";
const STEP_LOG_DIR_NAME: &str = "step-logs";
const OUTPUT_MANIFEST_NAME: &str = "sandbox-fs-tree.jsonl";

/// Builder implementation registered for recipe nodes tagged `SandboxInstall`.
#[derive(Debug)]
pub struct SandboxInstallBuilder;

static SANDBOX_INSTALL_BUILDER: SandboxInstallBuilder = SandboxInstallBuilder;

/// Builder classes provided by this crate.
pub static BUILDERS: &[&'static dyn Builder] = &[&SANDBOX_INSTALL_BUILDER];

/// Return runtime functions supported by `bobr-sandbox-install`.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![bobr_runtime::runtime_ns::NsFunction::new(
        SandboxInstallFunction,
    )]
}

/// Recipe-facing `SandboxInstall` builder config.
///
/// This shape intentionally matches the existing `Sandbox` config. The input
/// contract differs: `rootfs` is a materialized fs-tree root.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Tree of config files exposed to build steps; `{}` means none.
    #[serde(default = "default_script_config")]
    script_config: Value,
    /// Ordered command steps to execute inside the sandbox.
    steps: Vec<BuildStep>,
    /// Whether the output is captured as an ownership-aware fs-tree (the
    /// default). When `false`, the output tree is instead chowned to a single
    /// owner and captured as a plain object — for self-contained artifacts
    /// (e.g. images) where per-file ownership is irrelevant.
    #[serde(default = "default_preserve_ownership")]
    preserve_ownership: bool,
}

fn default_script_config() -> Value {
    Value::Object(Map::new())
}

fn default_preserve_ownership() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StepUser {
    BuildUser,
    Root,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuildStep {
    name: String,
    run_as: StepUser,
    cwd: String,
    argv: Vec<String>,
    #[serde(default)]
    env: Map<String, Value>,
}

static SANDBOX_SPEC: InputSpec = InputSpec {
    required_inputs: &["_rootfs"],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for SandboxInstallBuilder {
    type Config = SandboxConfig;

    fn tag(&self) -> &'static str {
        "SandboxInstall"
    }

    fn impl_version(&self) -> &'static str {
        // Bumped 1 -> 2: the sandbox now mounts a tmpfs at /dev/shm, which
        // changes the environment every sandbox build runs in (e.g. Python's
        // configure detects POSIX semaphores and builds _multiprocessing.SemLock).
        "2"
    }

    fn is_arch_dependent(&self) -> bool {
        true
    }

    fn spec(&self) -> &'static InputSpec {
        &SANDBOX_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError> {
        build_sandbox(config, inputs, cx)
    }
}

fn build_sandbox(
    config: SandboxConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<PathBuf, BuilderError> {
    validate_sandbox_config(&config).map_err(map_error)?;
    let rootfs = inputs.required("_rootfs")?.clone();
    validate_rootfs_path(&rootfs).map_err(map_error)?;
    let fs_tree = cx.fs_tree();

    let extra_inputs =
        collect_extra_inputs(&SANDBOX_SPEC, "SandboxInstall", &inputs).map_err(map_error)?;
    validate_step_interpolations(&config.steps, &extra_inputs).map_err(map_error)?;

    cx.log_event(BuildLogLevel::Info, "sandbox-install", "preparing inputs");
    let launcher_path = tools::resolve_and_preflight_sandbox_launcher()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let runtime_input =
        prepare_sandbox_input(&config, rootfs, extra_inputs, cx, fs_tree, launcher_path)
            .map_err(map_error)?;

    cx.log_event(
        BuildLogLevel::Info,
        "sandbox-install",
        format!(
            "running with {} step(s) and {} extra input(s)",
            runtime_input.steps.len(),
            runtime_input.extra_inputs.len()
        ),
    );

    let output = cx
        .runtime()
        .run(&SandboxInstallFunction, runtime_input)
        .map_err(|error| BuilderError::ExecutionFailed(format!("sandbox build failed: {error}")))?;
    write_build_report(cx, &output);

    // The runtime function produces its output artifact under cx.temp_dir and
    // guarantees it is host-owned; the builder stages it verbatim.
    Ok(output.output_path)
}

fn prepare_sandbox_input(
    config: &SandboxConfig,
    rootfs: PathBuf,
    extra_inputs: Vec<(String, PathBuf)>,
    cx: &BuildContext,
    fs_tree: FsTree,
    launcher_path: PathBuf,
) -> Result<SandboxInput, SandboxError> {
    let config_path = cx.temp_dir.join(CONFIG_DIR_NAME);
    recreate_empty_dir_force(&config_path)?;
    write_script_config(&config_path, &config.script_config)?;

    let sandbox_inputs = extra_inputs
        .iter()
        .map(|(name, input)| build_sandbox_input(name, input))
        .collect::<Result<Vec<_>, _>>()?;

    let sandbox_steps = config
        .steps
        .iter()
        .map(|step| build_sandbox_step(step, &extra_inputs, cx))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(SandboxInput {
        rootfs,
        config_dir: config_path,
        tmp: cx.temp_dir.clone(),
        fs_tree,
        launcher_path,
        extra_inputs: sandbox_inputs,
        steps: sandbox_steps,
        preserve_ownership: config.preserve_ownership,
        build_seed_hex: cx.build_seed().to_hex(),
    })
}

/// Build one runtime step config consumed by `SandboxInstallFunction`.
fn build_sandbox_step(
    step: &BuildStep,
    inputs: &[(String, PathBuf)],
    cx: &BuildContext,
) -> Result<SandboxRuntimeStep, SandboxError> {
    let cwd = PathBuf::from(resolve_step_cwd(step, inputs)?);
    let argv = resolve_step_argv(step, inputs)?;
    let env_overrides = resolve_step_env(step, inputs)?
        .into_iter()
        .collect::<HashMap<_, _>>();
    let logs = cx.temp_dir.join(STEP_LOG_DIR_NAME);
    fs::create_dir_all(&logs).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to create sandbox step log directory '{}': {error}",
            logs.display()
        ))
    })?;
    let log_name = sanitize_log_name(&step.name);
    let stdout_path = allocate_step_log_path(
        cx,
        &format!("sandbox-step-{log_name}-stdout"),
        logs.join(format!("{log_name}.stdout")),
    )?;
    let stderr_path = allocate_step_log_path(
        cx,
        &format!("sandbox-step-{log_name}-stderr"),
        logs.join(format!("{log_name}.stderr")),
    )?;

    Ok(SandboxRuntimeStep {
        name: step.name.clone(),
        run_as: step.run_as,
        cwd,
        argv,
        env_overrides,
        umask: DEFAULT_SANDBOX_UMASK,
        stdout_path,
        stderr_path,
    })
}

/// Allocate the host log file path reported for a step stream.
///
/// Build contexts normally allocate stable raw log paths. The fallback keeps
/// unit tests and minimal contexts functional without changing report shape.
fn allocate_step_log_path(
    cx: &BuildContext,
    label: &str,
    fallback: PathBuf,
) -> Result<PathBuf, SandboxError> {
    let path = match cx.allocate_raw_log_path(label) {
        Ok(path) => path,
        Err(_) => fallback,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            SandboxError::FsFailed(format!(
                "failed to create sandbox log directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    Ok(path)
}

/// Convert an arbitrary step name into a filesystem-safe log basename.
fn sanitize_log_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Write a high-level sandbox result log into the build context.
fn write_build_report(cx: &BuildContext, output: &SandboxOutput) {
    let steps = output
        .steps
        .iter()
        .map(|step| {
            let mut object = Map::new();
            object.insert("name".to_string(), Value::String(step.name.clone()));
            object.insert("run_as".to_string(), Value::String(step.run_as.clone()));
            object.insert("exit_code".to_string(), Value::from(step.exit_code));
            object.insert(
                "duration_ms".to_string(),
                Value::from(step.duration_ms as u64),
            );
            object.insert(
                "stdout_path".to_string(),
                Value::String(step.stdout_path.display().to_string()),
            );
            object.insert(
                "stderr_path".to_string(),
                Value::String(step.stderr_path.display().to_string()),
            );
            Value::Object(object)
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "steps": steps,
    });
    if let Ok(text) = serde_json::to_string_pretty(&report) {
        let log_path = cx.write_raw_log("sandbox-result", &text);
        cx.log_event_with_details(
            BuildLogLevel::Info,
            "sandbox-result",
            format!("sandbox ran {} step(s)", output.steps.len()),
            None,
            log_path,
            Map::new(),
        );
    }
}

#[derive(Debug)]
enum SandboxError {
    InvalidConfig(String),
    InputResolutionFailed(String),
    FsFailed(String),
}

impl SandboxError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::InputResolutionFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

fn map_error(error: SandboxError) -> BuilderError {
    match error {
        SandboxError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        SandboxError::InputResolutionFailed(message) | SandboxError::FsFailed(message) => {
            BuilderError::ExecutionFailed(message)
        }
    }
}

/// Validate the full recipe-facing config before host paths are prepared.
fn validate_sandbox_config(config: &SandboxConfig) -> Result<(), SandboxError> {
    validate_script_config(&config.script_config)?;
    validate_steps(&config.steps)
}

fn validate_rootfs_path(rootfs: &Path) -> Result<(), SandboxError> {
    if rootfs.is_dir() {
        Ok(())
    } else {
        Err(SandboxError::InputResolutionFailed(format!(
            "rootfs input must be a materialized fs-tree directory: '{}'",
            rootfs.display()
        )))
    }
}

/// Validate step shape without resolving interpolation.
fn validate_steps(steps: &[BuildStep]) -> Result<(), SandboxError> {
    if steps.is_empty() {
        return Err(SandboxError::InvalidConfig(
            "steps must contain at least one step".to_string(),
        ));
    }

    let mut seen_names = HashMap::new();
    let mut seen_log_names = HashMap::new();
    for (index, step) in steps.iter().enumerate() {
        if step.name.trim().is_empty() {
            return Err(SandboxError::InvalidConfig(format!(
                "steps[{index}].name must not be empty"
            )));
        }
        if let Some(previous) = seen_names.insert(step.name.as_str(), index) {
            return Err(SandboxError::InvalidConfig(format!(
                "steps[{index}].name '{}' duplicates steps[{previous}].name",
                step.name
            )));
        }
        let log_name = sanitize_log_name(&step.name);
        if let Some(previous) = seen_log_names.insert(log_name.clone(), index) {
            return Err(SandboxError::InvalidConfig(format!(
                "steps[{index}].name '{}' collides with steps[{previous}].name '{}' after log-name sanitization ('{log_name}')",
                step.name, steps[previous].name
            )));
        }
        if step.cwd.trim().is_empty() {
            return Err(SandboxError::InvalidConfig(format!(
                "steps[{index}].cwd must not be empty"
            )));
        }
        if step.argv.is_empty() {
            return Err(SandboxError::InvalidConfig(format!(
                "steps[{index}].argv must not be empty"
            )));
        }
        for (arg_index, arg) in step.argv.iter().enumerate() {
            if arg.is_empty() {
                return Err(SandboxError::InvalidConfig(format!(
                    "steps[{index}].argv[{arg_index}] must not be empty"
                )));
            }
        }
        validate_step_env(&step.env, &format!("steps[{index}].env"))?;
    }

    Ok(())
}

/// Validate that environment keys and values can be materialized.
fn validate_step_env(env: &Map<String, Value>, path: &str) -> Result<(), SandboxError> {
    for (key, value) in env {
        validate_env_key(key, path)?;
        if !matches!(value, Value::String(_)) {
            return Err(SandboxError::InvalidConfig(format!(
                "{path}.{key} must be a string"
            )));
        }
    }
    Ok(())
}

/// Validate a step environment key before passing it to the runner.
fn validate_env_key(key: &str, path: &str) -> Result<(), SandboxError> {
    if key.is_empty()
        || key == "."
        || key == ".."
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(SandboxError::InvalidConfig(format!(
            "env key '{}' at {} is invalid; allowed chars: [A-Za-z0-9._-]",
            key, path
        )));
    }
    Ok(())
}

/// Validate a recipe input name before using it as a mount path segment.
fn validate_input_name(name: &str) -> Result<(), SandboxError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(SandboxError::InvalidConfig(
            "input name must not be empty".to_string(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(SandboxError::InvalidConfig(format!(
            "input name '{name}' must start with an ASCII letter or underscore"
        )));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(SandboxError::InvalidConfig(format!(
            "input name '{name}' must contain only ASCII letters, digits, and underscores"
        )));
    }
    Ok(())
}

/// Return the absolute container path for a named extra input.
fn input_mount_path(name: &str) -> String {
    format!("{CONTAINER_INPUTS_DIR}/{name}")
}

fn build_sandbox_input(name: &str, input: &Path) -> Result<SandboxRuntimeInput, SandboxError> {
    let path = input.to_path_buf();
    if !path.is_dir() && !path.is_file() {
        return Err(SandboxError::InputResolutionFailed(format!(
            "sandbox input must resolve to a file or directory: {}",
            path.display()
        )));
    }
    Ok(SandboxRuntimeInput {
        name: name.to_string(),
        path,
    })
}

/// Collect and validate all extra inputs accepted by the `Sandbox` spec.
fn collect_extra_inputs(
    spec: &InputSpec,
    builder_name: &str,
    inputs: &BuilderInputs,
) -> Result<Vec<(String, PathBuf)>, SandboxError> {
    let mut named = Vec::new();
    for (name, object) in inputs.extras(spec) {
        validate_input_name(name)?;
        if matches!(name, "build" | "out" | "config") {
            return Err(SandboxError::InvalidConfig(format!(
                "input name '{name}' conflicts with a reserved {builder_name} interpolation variable"
            )));
        }
        named.push((name.to_string(), object.clone()));
    }
    Ok(named)
}

/// Render one step string by expanding `@{name}` interpolation variables.
fn interpolate_step_string(
    value: &str,
    inputs: &[(String, PathBuf)],
) -> Result<String, SandboxError> {
    let mut rendered = String::new();
    let mut index = 0;

    while index < value.len() {
        let rest = &value[index..];
        if let Some(after_escape) = rest.strip_prefix("@@{") {
            let Some(end) = after_escape.find('}') else {
                return Err(SandboxError::InvalidConfig(format!(
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
                return Err(SandboxError::InvalidConfig(format!(
                    "unterminated interpolation in '{value}'"
                )));
            };
            let key = &after_start[..end];
            validate_interpolation_name(key, value, false)?;
            rendered.push_str(&interpolation_value(key, inputs)?);
            index += 2 + end + 1;
            continue;
        }

        let ch = rest.chars().next().expect("rest is non-empty");
        rendered.push(ch);
        index += ch.len_utf8();
    }

    Ok(rendered)
}

/// Validate an interpolation or interpolation escape name.
fn validate_interpolation_name(key: &str, value: &str, escaped: bool) -> Result<(), SandboxError> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(SandboxError::InvalidConfig(format!(
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
        return Err(SandboxError::InvalidConfig(format!(
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

/// Resolve a built-in or named-input interpolation variable.
fn interpolation_value(key: &str, inputs: &[(String, PathBuf)]) -> Result<String, SandboxError> {
    match key {
        "build" => Ok(CONTAINER_BUILD_DIR.to_string()),
        "out" => Ok(CONTAINER_OUT_DIR.to_string()),
        "config" => Ok(CONTAINER_CONFIG_DIR.to_string()),
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
                SandboxError::InvalidConfig(format!("unknown interpolation variable '@{{{key}}}'"))
            }),
    }
}

/// Resolve and validate a step working directory.
fn resolve_step_cwd(
    step: &BuildStep,
    inputs: &[(String, PathBuf)],
) -> Result<String, SandboxError> {
    let cwd = interpolate_step_string(&step.cwd, inputs)?;
    if cwd.is_empty() || !cwd.starts_with('/') {
        return Err(SandboxError::InvalidConfig(format!(
            "step '{}' resolved cwd must be an absolute path, got '{}'",
            step.name, cwd
        )));
    }
    Ok(cwd)
}

/// Resolve all argv entries for a step.
fn resolve_step_argv(
    step: &BuildStep,
    inputs: &[(String, PathBuf)],
) -> Result<Vec<String>, SandboxError> {
    step.argv
        .iter()
        .map(|arg| interpolate_step_string(arg, inputs))
        .collect()
}

/// Resolve all environment values for a step.
fn resolve_step_env(
    step: &BuildStep,
    inputs: &[(String, PathBuf)],
) -> Result<Vec<(String, String)>, SandboxError> {
    let mut rendered = Vec::new();
    for (key, value) in &step.env {
        let string_value = value.as_str().ok_or_else(|| {
            SandboxError::InvalidConfig(format!(
                "step '{}' env key '{}' must be a string",
                step.name, key
            ))
        })?;
        rendered.push((key.clone(), interpolate_step_string(string_value, inputs)?));
    }
    Ok(rendered)
}

/// Eagerly validate interpolation in every step field.
fn validate_step_interpolations(
    steps: &[BuildStep],
    inputs: &[(String, PathBuf)],
) -> Result<(), SandboxError> {
    for step in steps {
        let _ = resolve_step_cwd(step, inputs)?;
        let _ = resolve_step_argv(step, inputs)?;
        let _ = resolve_step_env(step, inputs)?;
    }
    Ok(())
}

fn recreate_empty_dir_force(path: &Path) -> Result<(), SandboxError> {
    if fs::symlink_metadata(path).is_ok() {
        if path.is_dir() && !path.is_symlink() {
            remove_dir_force(path)?;
        } else {
            fs::remove_file(path).map_err(|error| {
                SandboxError::FsFailed(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

fn recreate_empty_dir(path: &Path) -> Result<(), SandboxError> {
    if fs::symlink_metadata(path).is_ok() {
        if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(path).map_err(|error| {
                SandboxError::FsFailed(format!(
                    "failed to remove previous directory '{}': {error}",
                    path.display()
                ))
            })?;
        } else {
            fs::remove_file(path).map_err(|error| {
                SandboxError::FsFailed(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

fn remove_dir_force(path: &Path) -> Result<(), SandboxError> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    make_tree_writable(path)?;
    fs::remove_dir_all(path).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to remove directory '{}': {error}",
            path.display()
        ))
    })
}

fn make_tree_writable(path: &Path) -> Result<(), SandboxError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to inspect path '{}': {error}",
            path.display()
        ))
    })?;

    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_dir() {
        let mode = metadata.permissions().mode();
        let desired = mode | 0o700;
        if desired != mode {
            fs::set_permissions(path, fs::Permissions::from_mode(desired)).map_err(|error| {
                SandboxError::FsFailed(format!(
                    "failed to adjust permissions for '{}': {error}",
                    path.display()
                ))
            })?;
        }

        for entry in fs::read_dir(path).map_err(|error| {
            SandboxError::FsFailed(format!(
                "failed to read directory '{}': {error}",
                path.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                SandboxError::FsFailed(format!(
                    "failed to read directory entry in '{}': {error}",
                    path.display()
                ))
            })?;
            make_tree_writable(&entry.path())?;
        }
    }

    Ok(())
}

/// Validate `script_config` before materializing it on disk. `{}` (or null)
/// means no config files.
fn validate_script_config(value: &Value) -> Result<(), SandboxError> {
    match value {
        Value::Null => Ok(()),
        value => validate_script_config_node(value, "<root>"),
    }
}

/// Recursively validate one `script_config` node.
fn validate_script_config_node(value: &Value, path: &str) -> Result<(), SandboxError> {
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
        _ => Err(SandboxError::InvalidConfig(format!(
            "script_config supports only records, arrays, and string leaves; invalid value at {path}"
        ))),
    }
}

/// Validate a `script_config` object key before using it as a path segment.
fn validate_script_config_key(key: &str, path: &str) -> Result<(), SandboxError> {
    if key.is_empty()
        || key == "."
        || key == ".."
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(SandboxError::InvalidConfig(format!(
            "script_config key '{}' at {} is invalid; allowed chars: [A-Za-z0-9._-]",
            key, path
        )));
    }
    Ok(())
}

/// Materialize `script_config` under `root`. `{}` (or null) writes nothing.
fn write_script_config(root: &Path, value: &Value) -> Result<(), SandboxError> {
    match value {
        Value::Null => Ok(()),
        value => write_script_config_node(root, value, "<root>"),
    }
}

/// Recursively write one `script_config` node to the host filesystem.
fn write_script_config_node(
    path: &Path,
    value: &Value,
    debug_path: &str,
) -> Result<(), SandboxError> {
    match value {
        Value::String(contents) => fs::write(path, contents).map_err(|error| {
            SandboxError::FsFailed(format!(
                "failed to write script_config leaf '{}' to '{}': {error}",
                debug_path,
                path.display()
            ))
        }),
        Value::Array(items) => {
            recreate_empty_dir(path)?;
            for (index, item) in items.iter().enumerate() {
                let child_path = path.join(format!("{index:08}"));
                write_script_config_node(&child_path, item, &format!("{debug_path}[{index}]"))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            recreate_empty_dir(path)?;
            for (key, item) in map {
                let child_path = path.join(key);
                write_script_config_node(&child_path, item, &format!("{debug_path}.{key}"))?;
            }
            Ok(())
        }
        _ => Err(SandboxError::InvalidConfig(format!(
            "script_config supports only records, arrays, and string leaves; invalid value at {debug_path}"
        ))),
    }
}

/// [`RuntimeFunction`] that runs a sandbox: sets up the container (rootfs,
/// mounts, namespaces) via `bobr-sandbox-launcher`, executes the configured
/// steps, and captures the result. Dispatched through a runtime, so it runs
/// in-process (host) or in a child user namespace.
#[derive(Debug, Clone, Copy)]
pub struct SandboxInstallFunction;

/// Typed input to [`SandboxInstallFunction`]: the rootfs, the caller-owned config
/// directory, a scratch `tmp` the function owns, the fs-tree to capture output
/// into, the launcher binary, the extra named inputs, and the ordered steps to
/// run. Serialized when the call is marshalled to a namespace worker.
///
/// The function creates its own working directories (build workspace and output
/// staging) under `tmp`, and guarantees that on return nothing owned by an
/// in-namespace sub-uid is left in `tmp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxInput {
    rootfs: PathBuf,
    config_dir: PathBuf,
    tmp: PathBuf,
    fs_tree: FsTree,
    launcher_path: PathBuf,
    extra_inputs: Vec<SandboxRuntimeInput>,
    steps: Vec<SandboxRuntimeStep>,
    /// Capture the output as an ownership-aware fs-tree (`true`), or chown it to
    /// a single owner and capture it as a plain object (`false`).
    preserve_ownership: bool,
    /// Deterministic per-build seed, exported to every step as
    /// `BOBR_BUILD_SEED` (64 lowercase hex chars).
    build_seed_hex: String,
}

/// A named extra input exposed inside the sandbox: a `name` and the host `path`
/// bind-mounted under it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxRuntimeInput {
    name: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SandboxRuntimeStep {
    name: String,
    run_as: StepUser,
    cwd: PathBuf,
    argv: Vec<String>,
    env_overrides: HashMap<String, String>,
    umask: u32,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

/// Typed output of [`SandboxInstallFunction`]: the host path of the produced artifact
/// (opaque to the caller — currently an fs-tree manifest) and a per-step
/// execution report. The artifact lives in the input `tmp` and is guaranteed
/// not to be owned by an in-namespace sub-uid.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxOutput {
    output_path: PathBuf,
    steps: Vec<SandboxStepReport>,
}

impl RuntimeFunction for SandboxInstallFunction {
    type Input = SandboxInput;
    type Output = SandboxOutput;

    fn name(&self) -> &'static str {
        "sandbox-install"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        if !input.rootfs.is_dir() {
            return Err(RuntimeError::new(format!(
                "SandboxInstall rootfs must be a directory: '{}'",
                input.rootfs.display()
            )));
        }

        // Create the working directories the function owns under `tmp`: the
        // build workspace (mount scaffolding) and the output staging dir.
        let workspace = input.tmp.join(RUNTIME_DIR_NAME);
        recreate_empty_dir_force(&workspace)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let out_dir = input.tmp.join(OUTPUT_DIR_NAME);
        recreate_empty_dir_force(&out_dir).map_err(|error| RuntimeError::new(error.to_string()))?;

        let result = run_sandbox_and_capture(&input, &workspace, &out_dir);

        // Guarantee no in-namespace-owned files remain in `tmp`. The workspace
        // (build tree) is always removed as ns-root. `out_dir` is consumed on
        // the fs-tree path and is the artifact on the plain path, so on success
        // it must stay — remove it only on failure, where ns files may remain
        // and the host user could not clear them afterwards.
        if result.is_err() {
            let _ = remove_dir_force(&out_dir);
        }
        let workspace_cleanup = remove_dir_force(&workspace);
        let output = result?;
        workspace_cleanup.map_err(|error| RuntimeError::new(error.to_string()))?;

        Ok(output)
    }
}

/// Runs the sandbox and captures its output: prepares the mounts, runs the
/// launcher, then captures the output tree per `preserve_ownership`. Its caller
/// owns cleaning `workspace`/`out_dir` regardless of the outcome.
fn run_sandbox_and_capture(
    input: &SandboxInput,
    workspace: &Path,
    out_dir: &Path,
) -> Result<SandboxOutput, RuntimeError> {
    let prepared = mounts::PreparedSandbox::create(input, workspace, out_dir)?;
    let steps = lifecycle::run_sandbox_launcher(
        &input.launcher_path,
        &prepared.launcher_config,
        &prepared.runtime_files.success_report,
        &prepared.runtime_files.failure_report,
    )?;

    let output_path = if input.preserve_ownership {
        // Ownership-aware fs-tree: intern the output (scans into fs-files and,
        // when fully fresh, moves the tree into the fs-trees cache, consuming
        // out_dir), then write the manifest as the host-owned artifact.
        let manifest = input
            .fs_tree
            .intern_tree(out_dir.to_path_buf())
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let output_path = input.tmp.join(OUTPUT_MANIFEST_NAME);
        manifest
            .write_canonical(&output_path)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        output_path
    } else {
        // Plain object: chown the whole tree to a single owner (ns-root → host
        // uid) so it is host-owned, and hand back the directory itself. Object
        // hashing ignores uid/gid, so the result is uid-independent.
        chown_tree_to_root(out_dir).map_err(|error| RuntimeError::new(error.to_string()))?;
        out_dir.to_path_buf()
    };

    Ok(SandboxOutput { output_path, steps })
}

/// Recursively chowns `path` to uid 0 / gid 0 (in-namespace root, which maps to
/// the host owner) without following symlinks, so the tree becomes host-owned.
fn chown_tree_to_root(path: &Path) -> Result<(), SandboxError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        SandboxError::FsFailed(format!("failed to inspect '{}': {error}", path.display()))
    })?;
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path).map_err(|error| {
            SandboxError::FsFailed(format!("failed to read '{}': {error}", path.display()))
        })? {
            let entry = entry.map_err(|error| {
                SandboxError::FsFailed(format!(
                    "failed to read entry in '{}': {error}",
                    path.display()
                ))
            })?;
            chown_tree_to_root(&entry.path())?;
        }
    }
    std::os::unix::fs::lchown(path, Some(0), Some(0)).map_err(|error| {
        SandboxError::FsFailed(format!("failed to chown '{}': {error}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_builder::Builder;
    use bobr_store::Store;
    use serde_json::json;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    // An `FsTree` from a throwaway store under `<root>/store` (there is no
    // public `FsTree` constructor — it comes only from a `Store`).
    fn store_fs_tree(root: &Path) -> FsTree {
        let store_root = root.join("store");
        fs::create_dir_all(&store_root).unwrap();
        Store::create(&store_root).unwrap().fs_tree()
    }

    fn valid_config() -> SandboxConfig {
        SandboxConfig {
            script_config: default_script_config(),
            steps: vec![BuildStep {
                name: "build".to_string(),
                run_as: StepUser::BuildUser,
                cwd: "/".to_string(),
                argv: vec!["true".to_string()],
                env: Map::new(),
            }],
            preserve_ownership: true,
        }
    }

    fn inputs(rootfs: PathBuf) -> BuilderInputs {
        BuilderInputs::new(BTreeMap::from([("_rootfs".to_string(), rootfs)]))
    }

    #[test]
    fn spec_requires_fs_tree_root_rootfs_and_allows_extra_inputs() {
        assert_eq!(TypedBuilder::tag(&SandboxInstallBuilder), "SandboxInstall");
        assert_eq!(SANDBOX_SPEC.required_inputs.len(), 1);
        assert_eq!(SANDBOX_SPEC.required_inputs[0], "_rootfs");
        assert!(SANDBOX_SPEC.optional_inputs.is_empty());
        assert!(SANDBOX_SPEC.allow_extra_inputs);
    }

    #[test]
    fn runtime_functions_registers_sandbox_function() {
        let functions = runtime_functions();

        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name(), "sandbox-install");
    }

    #[test]
    fn plan_rejects_unknown_config_fields() {
        static BUILDER: SandboxInstallBuilder = SandboxInstallBuilder;
        let error = BUILDER
            .plan(json!({"steps": [], "unexpected": true}))
            .err()
            .expect("plan should reject unknown config fields");

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn build_rejects_empty_steps() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("tmp"), store_fs_tree(temp.path()));

        let error = SandboxInstallBuilder
            .build_typed(
                SandboxConfig {
                    script_config: default_script_config(),
                    steps: Vec::new(),
                    preserve_ownership: true,
                },
                inputs(rootfs),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
        assert!(
            error
                .to_string()
                .contains("steps must contain at least one")
        );
    }

    #[test]
    fn prepare_runtime_input_resolves_steps_and_writes_script_config() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let rootfs = temp.path().join("rootfs");
        let source = temp.path().join("source");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&source).unwrap();
        let config = SandboxConfig {
            script_config: json!({"args": ["--flag"], "env": {"CC": "cc"}}),
            steps: vec![BuildStep {
                name: "compile/test".to_string(),
                run_as: StepUser::Root,
                cwd: "@{source}".to_string(),
                argv: vec!["@{config}/args/00000000".to_string()],
                env: Map::from_iter([("SRC".to_string(), Value::String("@{source}".to_string()))]),
            }],
            preserve_ownership: true,
        };
        let cx = BuildContext::with_noop_logger(temp.path().join("tmp"), store.fs_tree());
        fs::create_dir(&cx.temp_dir).unwrap();
        let extra_inputs = collect_extra_inputs(
            &SANDBOX_SPEC,
            "SandboxInstall",
            &BuilderInputs::new(BTreeMap::from([
                ("_rootfs".to_string(), rootfs.clone()),
                ("source".to_string(), source),
            ])),
        )
        .unwrap();

        let input = prepare_sandbox_input(
            &config,
            rootfs.clone(),
            extra_inputs,
            &cx,
            store.fs_tree(),
            PathBuf::from("/runner"),
        )
        .unwrap();

        assert_eq!(input.rootfs, rootfs);
        assert_eq!(input.tmp, cx.temp_dir);
        assert_eq!(input.steps.len(), 1);
        assert_eq!(
            input.steps[0].cwd,
            PathBuf::from(CONTAINER_INPUTS_DIR).join("source")
        );
        assert_eq!(
            input.steps[0].argv,
            vec![format!("{CONTAINER_CONFIG_DIR}/args/00000000")]
        );
        assert_eq!(
            input.steps[0].env_overrides["SRC"],
            format!("{CONTAINER_INPUTS_DIR}/source")
        );
        assert_eq!(
            fs::read_to_string(cx.temp_dir.join("config").join("args").join("00000000")).unwrap(),
            "--flag"
        );
    }

    #[test]
    fn build_reports_missing_rootfs_directory() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("missing-rootfs");
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("tmp"), store_fs_tree(temp.path()));

        let error = SandboxInstallBuilder
            .build_typed(valid_config(), inputs(rootfs), &mut cx)
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
        assert!(
            error
                .to_string()
                .contains("rootfs input must be a materialized fs-tree directory")
        );
    }
}
