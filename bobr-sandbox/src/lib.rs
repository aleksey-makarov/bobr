//! Sandbox builder backed by `bobr-runtime`.
//!
//! It provides the `Sandbox` builder that executes `bobr-sandbox-launcher`
//! through a `bobr-runtime` function and publishes fs-tree manifests.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod lifecycle;
mod mounts;
mod reports;
mod tools;

use bobr_builder::{
    BuildContext, Builder, BuilderError, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder,
};
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

/// Builder implementation registered for recipe nodes tagged `Sandbox`.
#[derive(Debug)]
pub struct SandboxBuilder;

static SANDBOX_BUILDER: SandboxBuilder = SandboxBuilder;

/// Builder classes provided by this crate.
pub static BUILDERS: &[&'static dyn Builder] = &[&SANDBOX_BUILDER];

/// Return runtime functions supported by `bobr-sandbox`.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![bobr_runtime::runtime_ns::NsFunction::new(SandboxFunction)]
}

/// Recipe-facing `Sandbox` builder config.
///
/// This shape intentionally matches the existing `Sandbox` config. The input
/// contract differs: `rootfs` is a materialized fs-tree root.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Optional tree of config files exposed to build steps.
    #[serde(default)]
    script_config: Option<Value>,
    /// Ordered command steps to execute inside the sandbox.
    steps: Vec<BuildStep>,
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

impl TypedBuilder for SandboxBuilder {
    type Config = SandboxConfig;

    fn tag(&self) -> &'static str {
        "Sandbox"
    }

    fn impl_version(&self) -> &'static str {
        "1"
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
    ) -> Result<StagedBuildResult, BuilderError> {
        build_sandbox(config, inputs, cx)
    }
}

fn build_sandbox(
    config: SandboxConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<StagedBuildResult, BuilderError> {
    validate_sandbox_config(&config).map_err(map_error)?;
    let rootfs = inputs.required("_rootfs")?.clone();
    validate_rootfs_path(&rootfs).map_err(map_error)?;
    let fs_tree = cx.fs_tree()?;

    let extra_inputs =
        collect_extra_inputs(&SANDBOX_SPEC, "Sandbox", &inputs).map_err(map_error)?;
    validate_step_interpolations(&config.steps, &extra_inputs).map_err(map_error)?;

    cx.log_event(BuildLogLevel::Info, "sandbox", "preparing inputs");
    let launcher_path = tools::resolve_and_preflight_sandbox_launcher()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let (runtime_input, output_manifest) =
        prepare_sandbox_input(&config, rootfs, extra_inputs, cx, fs_tree, launcher_path)
            .map_err(map_error)?;

    cx.log_event(
        BuildLogLevel::Info,
        "sandbox",
        format!(
            "running with {} step(s) and {} extra input(s)",
            runtime_input.steps.len(),
            runtime_input.extra_inputs.len()
        ),
    );

    let output = cx
        .runtime()
        .run(&SandboxFunction, runtime_input)
        .map_err(|error| BuilderError::ExecutionFailed(format!("sandbox build failed: {error}")))?;
    write_build_report(cx, &output);

    Ok(StagedBuildResult {
        staged_path: output_manifest,
    })
}

fn prepare_sandbox_input(
    config: &SandboxConfig,
    rootfs: PathBuf,
    extra_inputs: Vec<(String, PathBuf)>,
    cx: &BuildContext,
    fs_tree: FsTree,
    launcher_path: PathBuf,
) -> Result<(SandboxInput, PathBuf), SandboxError> {
    let output_path = cx.temp_dir.join(OUTPUT_DIR_NAME);
    recreate_empty_dir_force(&output_path)?;
    let config_path = cx.temp_dir.join(CONFIG_DIR_NAME);
    recreate_empty_dir_force(&config_path)?;
    write_script_config(&config_path, config.script_config.as_ref())?;

    let sandbox_inputs = extra_inputs
        .iter()
        .map(|(name, input)| build_sandbox_input(name, input))
        .collect::<Result<Vec<_>, _>>()?;

    let sandbox_steps = config
        .steps
        .iter()
        .map(|step| build_sandbox_step(step, &extra_inputs, cx))
        .collect::<Result<Vec<_>, _>>()?;

    let workspace = cx.temp_dir.join(RUNTIME_DIR_NAME);
    fs::create_dir_all(&workspace).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to create sandbox runtime workspace '{}': {error}",
            workspace.display()
        ))
    })?;

    let output_manifest = cx.temp_dir.join(OUTPUT_MANIFEST_NAME);
    if output_manifest.exists() {
        fs::remove_file(&output_manifest).map_err(|error| {
            SandboxError::FsFailed(format!(
                "failed to remove previous Sandbox manifest '{}': {error}",
                output_manifest.display()
            ))
        })?;
    }

    Ok((
        SandboxInput {
            rootfs,
            out_dir: output_path,
            config_dir: config_path,
            workspace,
            output_manifest: output_manifest.clone(),
            fs_tree,
            launcher_path,
            extra_inputs: sandbox_inputs,
            steps: sandbox_steps,
        },
        output_manifest,
    ))
}

/// Build one runtime step config consumed by `SandboxFunction`.
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
        "entries": output.entries,
        "steps": steps,
    });
    if let Ok(text) = serde_json::to_string_pretty(&report) {
        let log_path = cx.write_raw_log("sandbox-result", &text);
        cx.log_event_with_details(
            BuildLogLevel::Info,
            "sandbox-result",
            format!(
                "sandbox wrote fs-tree manifest with {} entries",
                output.entries
            ),
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
    validate_script_config(config.script_config.as_ref())?;
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

/// Validate `script_config` before materializing it on disk.
fn validate_script_config(value: Option<&Value>) -> Result<(), SandboxError> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => validate_script_config_node(value, "<root>"),
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

/// Materialize `script_config` under `root`.
fn write_script_config(root: &Path, value: Option<&Value>) -> Result<(), SandboxError> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => write_script_config_node(root, value, "<root>"),
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
pub struct SandboxFunction;

/// Typed input to [`SandboxFunction`]: the rootfs and output locations, the
/// fs-tree to capture as output, the launcher binary, the extra named inputs,
/// and the ordered steps to run. Serialized when the call is marshalled to a
/// namespace worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxInput {
    rootfs: PathBuf,
    out_dir: PathBuf,
    config_dir: PathBuf,
    workspace: PathBuf,
    output_manifest: PathBuf,
    fs_tree: FsTree,
    launcher_path: PathBuf,
    extra_inputs: Vec<SandboxRuntimeInput>,
    steps: Vec<SandboxRuntimeStep>,
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

/// Typed output of [`SandboxFunction`]: the number of captured fs-tree entries
/// and a per-step execution report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxOutput {
    entries: usize,
    steps: Vec<SandboxStepReport>,
}

impl RuntimeFunction for SandboxFunction {
    type Input = SandboxInput;
    type Output = SandboxOutput;

    fn name(&self) -> &'static str {
        "sandbox"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        if !input.rootfs.is_dir() {
            return Err(RuntimeError::new(format!(
                "Sandbox rootfs must be a directory: '{}'",
                input.rootfs.display()
            )));
        }

        let prepared = mounts::PreparedSandbox::create(&input)?;
        let steps = lifecycle::run_sandbox_launcher(
            &input.launcher_path,
            &prepared.launcher_config,
            &prepared.runtime_files.success_report,
            &prepared.runtime_files.failure_report,
        )?;
        let manifest = input
            .fs_tree
            .scan(&input.out_dir)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let entries = manifest.entries().len();
        manifest
            .write_canonical(&input.output_manifest)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let _ = fs::remove_dir_all(&prepared.dirs.root);

        Ok(SandboxOutput { entries, steps })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_builder::Builder;
    use bobr_store::Store;
    use serde_json::json;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn valid_config() -> SandboxConfig {
        SandboxConfig {
            script_config: None,
            steps: vec![BuildStep {
                name: "build".to_string(),
                run_as: StepUser::BuildUser,
                cwd: "/".to_string(),
                argv: vec!["true".to_string()],
                env: Map::new(),
            }],
        }
    }

    fn inputs(rootfs: PathBuf) -> BuilderInputs {
        BuilderInputs::new(BTreeMap::from([("_rootfs".to_string(), rootfs)]))
    }

    #[test]
    fn spec_requires_fs_tree_root_rootfs_and_allows_extra_inputs() {
        assert_eq!(TypedBuilder::tag(&SandboxBuilder), "Sandbox");
        assert_eq!(SANDBOX_SPEC.required_inputs.len(), 1);
        assert_eq!(SANDBOX_SPEC.required_inputs[0], "_rootfs");
        assert!(SANDBOX_SPEC.optional_inputs.is_empty());
        assert!(SANDBOX_SPEC.allow_extra_inputs);
    }

    #[test]
    fn runtime_functions_registers_sandbox_function() {
        let functions = runtime_functions();

        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name(), "sandbox");
    }

    #[test]
    fn erased_config_rejects_unknown_fields() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = SandboxBuilder
            .build_erased(
                json!({"steps": [], "unexpected": true}),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn build_rejects_empty_steps() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = SandboxBuilder
            .build_typed(
                SandboxConfig {
                    script_config: None,
                    steps: Vec::new(),
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
            script_config: Some(json!({"args": ["--flag"], "env": {"CC": "cc"}})),
            steps: vec![BuildStep {
                name: "compile/test".to_string(),
                run_as: StepUser::Root,
                cwd: "@{source}".to_string(),
                argv: vec!["@{config}/args/00000000".to_string()],
                env: Map::from_iter([("SRC".to_string(), Value::String("@{source}".to_string()))]),
            }],
        };
        let cx =
            BuildContext::with_noop_logger(temp.path().join("tmp")).with_fs_tree(store.fs_tree());
        fs::create_dir(&cx.temp_dir).unwrap();
        let extra_inputs = collect_extra_inputs(
            &SANDBOX_SPEC,
            "Sandbox",
            &BuilderInputs::new(BTreeMap::from([
                ("_rootfs".to_string(), rootfs.clone()),
                ("source".to_string(), source),
            ])),
        )
        .unwrap();

        let (input, output_manifest) = prepare_sandbox_input(
            &config,
            rootfs.clone(),
            extra_inputs,
            &cx,
            store.fs_tree(),
            PathBuf::from("/runner"),
        )
        .unwrap();

        assert_eq!(input.rootfs, rootfs);
        assert_eq!(
            output_manifest,
            temp.path().join("tmp").join("sandbox-fs-tree.jsonl")
        );
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
    fn build_requires_fs_tree() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = SandboxBuilder
            .build_typed(valid_config(), inputs(rootfs), &mut cx)
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires store fs-tree operations")
        );
    }

    #[test]
    fn build_reports_missing_rootfs_directory() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("missing-rootfs");
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = SandboxBuilder
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
