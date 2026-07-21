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
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Default file creation mask for sandbox steps.
const DEFAULT_SANDBOX_UMASK: u32 = 0o022;
const CONFIG_DIR_NAME: &str = "config";
const RUNTIME_DIR_NAME: &str = "runtime";
const STEP_LOG_DIR_NAME: &str = "step-logs";
const OUTPUT_MANIFEST_NAME: &str = "sandbox-fs-tree.jsonl";
/// Function-owned staging dir bind-mounted at `$out` for the `Sandbox` tag.
const OUTPUT_DIR_NAME: &str = "out";

/// Builder implementation registered for recipe nodes tagged `SandboxInstall`
/// (fs-tree, additive: the output is the overlay upper layer captured as a
/// delta to the build rootfs).
#[derive(Debug)]
pub struct SandboxInstallBuilder;

/// Builder implementation registered for recipe nodes tagged `Sandbox`
/// (plain-object: the same overlay run, but the output is whatever the build
/// installs into a `$out` staging dir, captured as a standalone object). Shares
/// the whole `SandboxInstall` machinery, diverging only at the capture seam.
#[derive(Debug)]
pub struct SandboxBuilder;

static SANDBOX_INSTALL_BUILDER: SandboxInstallBuilder = SandboxInstallBuilder;
static SANDBOX_BUILDER: SandboxBuilder = SandboxBuilder;

/// Builder classes provided by this crate.
pub static BUILDERS: &[&'static dyn Builder] = &[&SANDBOX_INSTALL_BUILDER, &SANDBOX_BUILDER];

/// Return runtime functions supported by `bobr-sandbox-install`.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![bobr_runtime::runtime_ns::NsFunction::new(
        SandboxInstallFunction,
    )]
}

/// Recipe-facing `SandboxInstall` builder config.
///
/// The build runs with a read-write overlay root; the output is whatever it
/// installs into that root, captured as an additive fs-tree layer. The `rootfs`
/// input is a materialized fs-tree root.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Tree of config files exposed to build steps; `{}` means none.
    #[serde(default = "default_script_config")]
    script_config: Value,
    /// Ordered command steps to execute inside the sandbox.
    steps: Vec<BuildStep>,
}

fn default_script_config() -> Value {
    Value::Object(Map::new())
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
        // Bumped 2 -> 3: the build runs with a read-write overlay root and its
        // output is the additive upper layer captured as an fs-tree; there is no
        // $out / BOBR_OUT_DIR mount or interpolation.
        // Bumped 3 -> 4: PYTHONDONTWRITEBYTECODE=1 added to the base env, which
        // suppresses non-reproducible import-time .pyc in the build output.
        // Bumped 4 -> 5: the captured upper layer is now pruned of pure copy-up
        // noise -- lower directories the build only touched (e.g. autoconf
        // probing /var/tmp for writability) no longer appear in the output.
        "5"
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
        build_sandbox(config, inputs, cx, false)
    }
}

impl TypedBuilder for SandboxBuilder {
    type Config = SandboxConfig;

    fn tag(&self) -> &'static str {
        "Sandbox"
    }

    fn impl_version(&self) -> &'static str {
        // History carried over from the standalone bobr-sandbox crate (whose
        // `Sandbox` builder this replaces):
        // Bumped 1 -> 2: the sandbox now mounts a tmpfs at /dev/shm, which
        // changes the environment every sandbox build runs in (e.g. Python's
        // configure detects POSIX semaphores and builds _multiprocessing.SemLock).
        // Bumped 2 -> 3: PYTHONDONTWRITEBYTECODE=1 added to the base env, which
        // suppresses non-reproducible import-time .pyc in the build output.
        // Bumped 3 -> 4: the build now runs with the same read-write overlay root
        // as SandboxInstall (plus a writable `$out` bind), dropping the old
        // per-entry read-only bind mount scheme; the output is still `$out`,
        // chowned to a single owner and captured as a standalone object.
        "4"
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
        build_sandbox(config, inputs, cx, true)
    }
}

fn build_sandbox(
    config: SandboxConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    plain_object: bool,
) -> Result<PathBuf, BuilderError> {
    let tag_name = if plain_object {
        "Sandbox"
    } else {
        "SandboxInstall"
    };
    let log_component = if plain_object {
        "sandbox"
    } else {
        "sandbox-install"
    };
    validate_sandbox_config(&config).map_err(map_error)?;
    let rootfs = inputs.required("_rootfs")?.clone();
    validate_rootfs_path(&rootfs).map_err(map_error)?;
    let fs_tree = cx.fs_tree();

    let extra_inputs = collect_extra_inputs(&SANDBOX_SPEC, tag_name, &inputs).map_err(map_error)?;
    validate_step_interpolations(&config.steps, &extra_inputs, plain_object).map_err(map_error)?;

    cx.log_event(BuildLogLevel::Info, log_component, "preparing inputs");
    let launcher_path = tools::resolve_and_preflight_sandbox_launcher()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let runtime_input = prepare_sandbox_input(
        &config,
        rootfs,
        extra_inputs,
        cx,
        fs_tree,
        launcher_path,
        plain_object,
    )
    .map_err(map_error)?;

    cx.log_event(
        BuildLogLevel::Info,
        log_component,
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
    plain_object: bool,
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
        .map(|step| build_sandbox_step(step, &extra_inputs, cx, plain_object))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(SandboxInput {
        rootfs,
        config_dir: config_path,
        tmp: cx.temp_dir.clone(),
        fs_tree,
        launcher_path,
        extra_inputs: sandbox_inputs,
        steps: sandbox_steps,
        build_seed_hex: cx.build_seed().to_hex(),
        plain_object,
    })
}

/// Build one runtime step config consumed by `SandboxInstallFunction`.
fn build_sandbox_step(
    step: &BuildStep,
    inputs: &[(String, PathBuf)],
    cx: &BuildContext,
    plain_object: bool,
) -> Result<SandboxRuntimeStep, SandboxError> {
    let cwd = PathBuf::from(resolve_step_cwd(step, inputs, plain_object)?);
    let argv = resolve_step_argv(step, inputs, plain_object)?;
    let env_overrides = resolve_step_env(step, inputs, plain_object)?
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
        if matches!(name, "build" | "config" | "out") {
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
    plain_object: bool,
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
            rendered.push_str(&interpolation_value(key, inputs, plain_object)?);
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

/// Resolve a built-in or named-input interpolation variable. `@{out}` is only
/// defined for the plain-object `Sandbox` tag (which mounts `$out`); on the
/// additive `SandboxInstall` tag it falls through to the unknown-variable error.
fn interpolation_value(
    key: &str,
    inputs: &[(String, PathBuf)],
    plain_object: bool,
) -> Result<String, SandboxError> {
    match key {
        "build" => Ok(CONTAINER_BUILD_DIR.to_string()),
        "config" => Ok(CONTAINER_CONFIG_DIR.to_string()),
        "out" if plain_object => Ok(CONTAINER_OUT_DIR.to_string()),
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
    plain_object: bool,
) -> Result<String, SandboxError> {
    let cwd = interpolate_step_string(&step.cwd, inputs, plain_object)?;
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
    plain_object: bool,
) -> Result<Vec<String>, SandboxError> {
    step.argv
        .iter()
        .map(|arg| interpolate_step_string(arg, inputs, plain_object))
        .collect()
}

/// Resolve all environment values for a step.
fn resolve_step_env(
    step: &BuildStep,
    inputs: &[(String, PathBuf)],
    plain_object: bool,
) -> Result<Vec<(String, String)>, SandboxError> {
    let mut rendered = Vec::new();
    for (key, value) in &step.env {
        let string_value = value.as_str().ok_or_else(|| {
            SandboxError::InvalidConfig(format!(
                "step '{}' env key '{}' must be a string",
                step.name, key
            ))
        })?;
        rendered.push((
            key.clone(),
            interpolate_step_string(string_value, inputs, plain_object)?,
        ));
    }
    Ok(rendered)
}

/// Eagerly validate interpolation in every step field.
fn validate_step_interpolations(
    steps: &[BuildStep],
    inputs: &[(String, PathBuf)],
    plain_object: bool,
) -> Result<(), SandboxError> {
    for step in steps {
        let _ = resolve_step_cwd(step, inputs, plain_object)?;
        let _ = resolve_step_argv(step, inputs, plain_object)?;
        let _ = resolve_step_env(step, inputs, plain_object)?;
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
    /// Deterministic per-build seed, exported to every step as
    /// `BOBR_BUILD_SEED` (64 lowercase hex chars).
    build_seed_hex: String,
    /// Plain-object (`Sandbox`) vs additive fs-tree (`SandboxInstall`) capture.
    /// When set, the run gets a writable `$out` bind and the output is `$out`
    /// captured as a standalone object; when clear, the overlay upper is
    /// captured as an additive fs-tree delta.
    plain_object: bool,
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

/// Typed output of [`SandboxInstallFunction`]: the host path of the produced
/// fs-tree manifest and a per-step execution report. The manifest lives in the
/// input `tmp` and is guaranteed not to be owned by an in-namespace sub-uid.
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

        // Create the working directories the function owns under `tmp`: the build
        // workspace (mount scaffolding), and -- for the plain-object `Sandbox`
        // path -- the `$out` staging dir bound writable inside the sandbox. The
        // additive path captures the overlay upper instead and needs no `$out`.
        let workspace = input.tmp.join(RUNTIME_DIR_NAME);
        recreate_empty_dir_force(&workspace)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let out_dir = if input.plain_object {
            let out = input.tmp.join(OUTPUT_DIR_NAME);
            recreate_empty_dir_force(&out).map_err(|error| RuntimeError::new(error.to_string()))?;
            Some(out)
        } else {
            None
        };

        let result = run_sandbox_and_capture(&input, &workspace, out_dir.as_deref());

        // Guarantee no in-namespace-owned files remain in `tmp`. The workspace
        // (build tree) is always removed as ns-root. `$out` is the artifact on
        // success (must stay) but may hold ns-owned files on failure, where the
        // host user could not clear them -- so remove it only on failure.
        if result.is_err()
            && let Some(out) = &out_dir
        {
            let _ = remove_dir_force(out);
        }
        let workspace_cleanup = remove_dir_force(&workspace);
        let output = result?;
        workspace_cleanup.map_err(|error| RuntimeError::new(error.to_string()))?;

        Ok(output)
    }
}

/// Runs the sandbox and captures its output: prepares the mounts (always a
/// read-write overlay root, plus a writable `$out` bind for the plain-object
/// path), runs the launcher, then captures the result. With `out_dir` (the
/// plain-object `Sandbox` path) the output is `$out` itself; without it (the
/// additive `SandboxInstall` path) it is the overlay upper as an fs-tree delta.
/// Its caller owns cleaning `workspace`/`out_dir` regardless of the outcome.
fn run_sandbox_and_capture(
    input: &SandboxInput,
    workspace: &Path,
    out_dir: Option<&Path>,
) -> Result<SandboxOutput, RuntimeError> {
    let prepared = mounts::PreparedSandbox::create(input, workspace, out_dir)?;
    let steps = lifecycle::run_sandbox_launcher(
        &input.launcher_path,
        &prepared.launcher_config,
        &prepared.runtime_files.success_report,
        &prepared.runtime_files.failure_report,
    )?;

    let output_path = if let Some(out_dir) = out_dir {
        // Plain object: chown the whole `$out` tree to a single owner (ns-root ->
        // host uid) so it is host-owned, and hand back the directory itself.
        // Object hashing ignores uid/gid, so the result is uid-independent. The
        // overlay upper (temporary writes to the live root) is simply discarded.
        chown_tree_to_root(out_dir).map_err(|error| RuntimeError::new(error.to_string()))?;
        out_dir.to_path_buf()
    } else {
        // Additive fs-tree: the build's output is the overlay upper layer. Drop
        // the sandbox's own scaffolding, verify the layer is purely additive,
        // prune pure copy-up noise (lower dirs the build only touched, e.g.
        // autoconf probing /var/tmp), then intern it (scans into fs-files and,
        // when fully fresh, moves the tree into the fs-trees cache, consuming
        // upper) and write the host-owned manifest.
        mounts::strip_overlay_scaffolding(&prepared.upper)?;
        validate_additive_layer(&prepared.upper, &input.rootfs)?;
        prune_passthrough_noise(&prepared.upper, &input.rootfs)?;
        let manifest = input
            .fs_tree
            .intern_tree(prepared.upper.clone())
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let output_path = input.tmp.join(OUTPUT_MANIFEST_NAME);
        manifest
            .write_canonical(&output_path)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        output_path
    };

    Ok(SandboxOutput { output_path, steps })
}

/// Recursively chowns `path` to uid 0 / gid 0 (in-namespace root, which maps to
/// the host owner) without following symlinks, so the plain-object tree becomes
/// host-owned. Object hashing ignores uid/gid, so the hash is unaffected.
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

/// Verifies that the overlay upper layer holds only additions relative to the
/// lower rootfs. Any non-additive change fails the build: overlay whiteouts
/// (deletions), opaque directories (a replaced directory), and files, symlinks,
/// or directories that already exist in the lower rootfs (a modification).
/// Scaffolding must already have been stripped from `upper`.
fn validate_additive_layer(upper: &Path, lower: &Path) -> Result<(), RuntimeError> {
    validate_additive_dir(upper, upper, lower)
}

fn validate_additive_dir(dir: &Path, upper_root: &Path, lower: &Path) -> Result<(), RuntimeError> {
    let mut entries = fs::read_dir(dir)
        .map_err(|error| {
            RuntimeError::new(format!("read overlay upper '{}': {error}", dir.display()))
        })?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(|error| RuntimeError::new(error.to_string()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)
            .map_err(|error| RuntimeError::new(format!("inspect '{}': {error}", path.display())))?;
        let file_type = meta.file_type();
        let relative = path.strip_prefix(upper_root).unwrap_or(&path);
        let shown = Path::new("/").join(relative).display().to_string();
        let lower_meta = fs::symlink_metadata(lower.join(relative)).ok();

        // Overlay whiteout: a character device with rdev 0:0 marks a deletion.
        if file_type.is_char_device() && meta.rdev() == 0 {
            return Err(RuntimeError::new(format!(
                "SandboxInstall build deleted '{shown}'; the builder only allows additions"
            )));
        }

        if file_type.is_dir() {
            if is_opaque_dir(&path)? {
                return Err(RuntimeError::new(format!(
                    "SandboxInstall build replaced directory '{shown}'; the builder only allows additions"
                )));
            }
            if let Some(lower_meta) = &lower_meta {
                // A directory shared with the lower rootfs is a legitimate
                // passthrough parent of an addition only when it is unchanged.
                if !lower_meta.is_dir()
                    || lower_meta.uid() != meta.uid()
                    || lower_meta.gid() != meta.gid()
                    || lower_meta.mode() & 0o7777 != meta.mode() & 0o7777
                {
                    return Err(RuntimeError::new(format!(
                        "SandboxInstall build modified existing directory '{shown}'; the builder only allows additions"
                    )));
                }
            }
            validate_additive_dir(&path, upper_root, lower)?;
        } else if lower_meta.is_some() {
            // A regular file or symlink whose path already exists below the
            // overlay: the build modified an existing entry.
            return Err(RuntimeError::new(format!(
                "SandboxInstall build modified existing '{shown}'; the builder only allows additions"
            )));
        }
    }
    Ok(())
}

/// Removes pure copy-up noise from the validated upper layer: directories that
/// also exist in the lower rootfs and, once their children are processed, hold
/// no genuine additions. Overlayfs copies a lower directory up whenever a build
/// merely touches something inside it -- e.g. autoconf probing `/var/tmp` for
/// writability -- leaving an unchanged passthrough directory in the upper that
/// is not part of the package's output. For an additive-into-`/` build such a
/// dir is harmless (it re-merges over the identical lower dir), but a
/// `*StageRootfs` build re-roots the upper at `/stage`, and these stray dirs
/// then sit outside it. Prune bottom-up so an emptied parent is pruned in turn.
/// A directory that is new (absent from the lower) or that still holds any entry
/// is kept, so genuine output -- including a file mis-installed outside the
/// intended prefix -- always survives to be seen downstream. Scaffolding must
/// already be stripped and the layer validated additive.
fn prune_passthrough_noise(upper: &Path, lower: &Path) -> Result<(), RuntimeError> {
    prune_passthrough_dir(upper, upper, lower)?;
    Ok(())
}

/// Prunes empty passthrough child directories of `dir` (see
/// [`prune_passthrough_noise`]) and returns whether `dir` itself is empty
/// afterwards, so the caller can prune it in turn.
fn prune_passthrough_dir(
    dir: &Path,
    upper_root: &Path,
    lower: &Path,
) -> Result<bool, RuntimeError> {
    let entries = fs::read_dir(dir)
        .map_err(|error| {
            RuntimeError::new(format!("read overlay upper '{}': {error}", dir.display()))
        })?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(|error| RuntimeError::new(error.to_string()))?;

    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)
            .map_err(|error| RuntimeError::new(format!("inspect '{}': {error}", path.display())))?;
        if !meta.file_type().is_dir() {
            continue;
        }
        // Prune the child's subtree first; only an emptied child can be removed.
        if !prune_passthrough_dir(&path, upper_root, lower)? {
            continue;
        }
        let relative = path.strip_prefix(upper_root).unwrap_or(&path);
        let in_lower = fs::symlink_metadata(lower.join(relative))
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        if in_lower {
            fs::remove_dir(&path).map_err(|error| {
                RuntimeError::new(format!("prune passthrough '{}': {error}", path.display()))
            })?;
        }
    }

    let empty = fs::read_dir(dir)
        .map_err(|error| {
            RuntimeError::new(format!("read overlay upper '{}': {error}", dir.display()))
        })?
        .next()
        .is_none();
    Ok(empty)
}

/// Returns whether `dir` carries an overlayfs opaque marker, set when a build
/// replaces a whole directory. Unprivileged overlays store it in the `user.*`
/// xattr namespace, privileged ones in `trusted.*`.
fn is_opaque_dir(dir: &Path) -> Result<bool, RuntimeError> {
    for name in ["user.overlay.opaque", "trusted.overlay.opaque"] {
        if xattr_equals(dir, name, b"y")? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reads an extended attribute and compares it to `expected`. A missing
/// attribute, an oversized value, or a filesystem without xattr support all
/// read as "not equal".
fn xattr_equals(path: &Path, name: &str, expected: &[u8]) -> Result<bool, RuntimeError> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RuntimeError::new(format!("path contains NUL byte: '{}'", path.display())))?;
    let c_name = CString::new(name).expect("xattr name has no interior NUL");
    let mut buf = [0_u8; 16];
    let len = unsafe {
        libc::lgetxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if len < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::ENOTSUP) | Some(libc::ERANGE) => Ok(false),
            _ => Err(RuntimeError::new(format!(
                "read xattr '{name}' of '{}': {error}",
                path.display()
            ))),
        };
    }
    Ok(&buf[..len as usize] == expected)
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
            false,
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

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    // Sets an xattr; returns false if the filesystem does not support it.
    fn try_set_xattr(path: &Path, name: &str, value: &[u8]) -> bool {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let c_name = CString::new(name).unwrap();
        let rc = unsafe {
            libc::lsetxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                0,
            )
        };
        rc == 0
    }

    #[test]
    fn additive_layer_accepts_additions_and_passthrough_dirs() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let upper = temp.path().join("upper");
        // Lower has /usr; upper carries it through unchanged plus new entries.
        fs::create_dir_all(lower.join("usr")).unwrap();
        fs::create_dir_all(upper.join("usr/lib")).unwrap();
        fs::write(upper.join("usr/lib/libfoo.so"), b"x").unwrap();
        fs::write(upper.join("newtop.txt"), b"y").unwrap();

        validate_additive_layer(&upper, &lower).unwrap();
    }

    #[test]
    fn additive_layer_rejects_modified_file() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let upper = temp.path().join("upper");
        fs::create_dir_all(lower.join("etc")).unwrap();
        fs::write(lower.join("etc/hosts"), b"orig").unwrap();
        fs::create_dir_all(upper.join("etc")).unwrap();
        fs::write(upper.join("etc/hosts"), b"changed").unwrap();

        let error = validate_additive_layer(&upper, &lower).unwrap_err();
        assert!(error.to_string().contains("modified existing"));
        assert!(error.to_string().contains("/etc/hosts"));
    }

    #[test]
    fn additive_layer_rejects_changed_directory_metadata() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let upper = temp.path().join("upper");
        fs::create_dir_all(lower.join("opt")).unwrap();
        set_mode(&lower.join("opt"), 0o755);
        fs::create_dir_all(upper.join("opt")).unwrap();
        set_mode(&upper.join("opt"), 0o700);

        let error = validate_additive_layer(&upper, &lower).unwrap_err();
        assert!(error.to_string().contains("modified existing directory"));
        assert!(error.to_string().contains("/opt"));
    }

    #[test]
    fn additive_layer_rejects_opaque_directory() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let upper = temp.path().join("upper");
        fs::create_dir_all(&lower).unwrap();
        fs::create_dir_all(upper.join("etc")).unwrap();
        if !try_set_xattr(&upper.join("etc"), "user.overlay.opaque", b"y") {
            eprintln!("additive_layer_rejects_opaque_directory: skipped (no xattr support)");
            return;
        }

        let error = validate_additive_layer(&upper, &lower).unwrap_err();
        assert!(error.to_string().contains("replaced directory"));
        assert!(error.to_string().contains("/etc"));
    }

    #[test]
    fn prune_removes_empty_passthrough_dir() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let upper = temp.path().join("upper");
        // Lower has /var/tmp; the build only copied it up unchanged (an autoconf
        // writability probe) while installing a real file under /stage.
        fs::create_dir_all(lower.join("var/tmp")).unwrap();
        fs::create_dir_all(upper.join("var/tmp")).unwrap();
        fs::create_dir_all(upper.join("stage/usr/bin")).unwrap();
        fs::write(upper.join("stage/usr/bin/patch"), b"x").unwrap();

        prune_passthrough_noise(&upper, &lower).unwrap();

        assert!(
            !upper.join("var").exists(),
            "empty passthrough /var is pruned"
        );
        assert!(
            upper.join("stage/usr/bin/patch").exists(),
            "the real addition survives"
        );
    }

    #[test]
    fn prune_keeps_passthrough_dir_holding_addition() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let upper = temp.path().join("upper");
        // /usr is a passthrough parent, but it holds a genuine addition.
        fs::create_dir_all(lower.join("usr")).unwrap();
        fs::create_dir_all(upper.join("usr/lib")).unwrap();
        fs::write(upper.join("usr/lib/libfoo.so"), b"x").unwrap();

        prune_passthrough_noise(&upper, &lower).unwrap();

        assert!(
            upper.join("usr/lib/libfoo.so").exists(),
            "the addition survives"
        );
        assert!(
            upper.join("usr").is_dir(),
            "its passthrough parent survives"
        );
    }

    #[test]
    fn prune_keeps_new_empty_dir() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let upper = temp.path().join("upper");
        // /opt exists in the lower, but /opt/created is a brand-new empty dir the
        // build genuinely produced -- absent from the lower, so it must survive
        // (this is what keeps a real mis-install visible, not hidden).
        fs::create_dir_all(lower.join("opt")).unwrap();
        fs::create_dir_all(upper.join("opt/created")).unwrap();

        prune_passthrough_noise(&upper, &lower).unwrap();

        assert!(
            upper.join("opt/created").is_dir(),
            "a new empty dir survives"
        );
    }

    #[test]
    fn strip_overlay_scaffolding_removes_injected_top_level() {
        let temp = tempdir().unwrap();
        let upper = temp.path().join("upper");
        for name in ["__bobr", "dev", "proc", "run", "tmp", "usr"] {
            fs::create_dir_all(upper.join(name)).unwrap();
        }
        fs::write(upper.join("dev/null"), b"").unwrap();
        fs::write(upper.join("usr/foo"), b"x").unwrap();

        mounts::strip_overlay_scaffolding(&upper).unwrap();

        for name in ["__bobr", "dev", "proc", "run", "tmp"] {
            assert!(!upper.join(name).exists(), "{name} should be stripped");
        }
        assert!(upper.join("usr/foo").exists());
    }

    /// Deletes a lower file through a real overlay so the kernel writes a
    /// whiteout into upper, then confirms validation rejects it. Forks first:
    /// `unshare(CLONE_NEWUSER)` needs a single-threaded process. Skips where
    /// unprivileged user namespaces are unavailable.
    #[test]
    fn additive_layer_rejects_whiteout_from_real_overlay() {
        let temp = tempdir().unwrap();
        let base = temp.path();
        let lower = base.join("lower");
        let upper = base.join("upper");
        let work = base.join("work");
        let mnt = base.join("mnt");
        for dir in [&lower, &upper, &work, &mnt] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(lower.join("victim.txt"), b"bye\n").unwrap();

        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let data = CString::new(format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        ))
        .unwrap();
        let mnt_c = CString::new(mnt.as_os_str().as_bytes()).unwrap();
        let overlay = CString::new("overlay").unwrap();

        // SAFETY: the child only makes syscalls and glibc-fork-safe allocations,
        // then exits without returning into the test harness.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let code = (|| -> i32 {
                if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
                    return 42;
                }
                if fs::write("/proc/self/setgroups", "deny").is_err()
                    || fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).is_err()
                    || fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).is_err()
                {
                    return 46;
                }
                let rc = unsafe {
                    libc::mount(
                        overlay.as_ptr(),
                        mnt_c.as_ptr(),
                        overlay.as_ptr(),
                        0,
                        data.as_ptr() as *const libc::c_void,
                    )
                };
                if rc != 0 {
                    return 43;
                }
                if fs::remove_file(mnt.join("victim.txt")).is_err() {
                    return 44;
                }
                // The whiteout now lives in upper; validation must reject it.
                if validate_additive_layer(&upper, &lower).is_ok() {
                    return 45;
                }
                0
            })();
            std::process::exit(code);
        }

        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        match code {
            0 => {}
            42 => eprintln!(
                "additive_layer_rejects_whiteout_from_real_overlay: skipped \
                 (unprivileged user namespaces unavailable)"
            ),
            other => panic!(
                "whiteout child failed with code {other} (43 mount, 44 rm, 45 not-rejected, 46 idmap)"
            ),
        }
    }
}
