//! Recipe-facing `Sandbox` builder implementation.
//!
//! This crate owns the `Sandbox` builder surface that recipe authors see:
//! parsing builder config, validating step definitions, resolving input
//! interpolation, materializing `script_config`, and staging the final sandbox
//! output as an fs-tree object.
//!
//! It deliberately does not implement namespace or mount setup. Once a recipe
//! is lowered into [`SandboxBuildConfig`], execution is delegated to
//! `mbuild-runtime`, which prepares the launcher workspace and runs
//! `mbuild-sandbox-runner`.
//!
//! The main flow is:
//!
//! 1. Validate the recipe-facing `SandboxConfig`.
//! 2. Resolve named inputs to stable container paths under `/__mbuild/inputs`.
//! 3. Write `script_config` into a host directory that is mounted read-only.
//! 4. Build `SandboxBuildConfig` for `mbuild-runtime`.
//! 5. Stage the runtime output directory as an fs-tree object.

use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    FsTreeOwnerMap, StagedBuildResult, TypedBuilder, fsutil, validate_fs_tree_object,
};
use mbuild_runtime::{
    SandboxBuildConfig, SandboxInput, SandboxRunAs, SandboxStep, cached_host_idmap,
    run_sandbox_build,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Builder implementation registered for recipe nodes tagged `Sandbox`.
pub struct SandboxBuilder;

// Host-side name of the raw output directory before fs-tree staging.
const OUTPUT_DIR_NAME: &str = "out";
// Host-side name of the directory containing materialized `script_config`.
const CONFIG_DIR_NAME: &str = "config";
// Container path prefix used for recipe inputs other than `rootfs`.
const INPUT_MOUNT_ROOT: &str = "/__mbuild/inputs";
// Container path where materialized `script_config` is mounted.
const CONFIG_MOUNT_PATH: &str = "/__mbuild/config";
// Container path of the writable build directory.
const BUILD_DIR_MOUNT_PATH: &str = "/__mbuild/build";
// Container path of the writable output directory.
const OUT_DIR_MOUNT_PATH: &str = "/__mbuild/out";
// Name of the staged fs-tree object directory inside the node temp dir.
const FS_TREE_OBJECT_DIR_NAME: &str = "fs-tree-object";

/// Recipe-facing `Sandbox` builder config.
///
/// `script_config` is an optional JSON tree with string leaves. It is written
/// to files before execution and mounted at `/__mbuild/config`. `steps` are
/// resolved into process executions inside the sandbox.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Optional tree of config files exposed to build steps.
    #[serde(default)]
    script_config: Option<Value>,
    /// Ordered command steps to execute inside the sandbox.
    steps: Vec<BuildStep>,
}

/// User identity requested for one sandbox step.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StepUser {
    /// Run as the sandbox build user, currently logical uid/gid `1:1`.
    BuildUser,
    /// Run as root inside the sandbox user namespace.
    Root,
}

/// One recipe-facing process step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildStep {
    /// Stable step name used in reports and log filenames.
    name: String,
    /// User identity for process execution.
    run_as: StepUser,
    /// Working directory before interpolation.
    cwd: String,
    /// Command argv before interpolation.
    argv: Vec<String>,
    /// Additional string environment variables before interpolation.
    #[serde(default)]
    env: Map<String, Value>,
}

// Static builder contract advertised to the mbuild recipe runtime.
static SANDBOX_SPEC: BuilderSpec = BuilderSpec {
    tag: "Sandbox",
    required_inputs: &["rootfs"],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

// Internal error categories before they are mapped to `BuilderError`.
#[derive(Debug)]
enum SandboxError {
    /// The recipe config is structurally invalid.
    InvalidConfig(String),
    /// A recipe input did not resolve to a usable host path.
    InputResolutionFailed(String),
    /// Sandbox execution or output staging failed.
    BuildFailed(String),
    /// Host filesystem preparation failed.
    FsFailed(String),
}

impl SandboxError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::InputResolutionFailed(message)
            | Self::BuildFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type BResult<T> = Result<T, SandboxError>;

impl TypedBuilder for SandboxBuilder {
    type Config = SandboxConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &SANDBOX_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_sandbox_config(&config).map_err(map_error)?;
        let rootfs = inputs.required("rootfs")?;
        let idmap = cached_host_idmap().map_err(|error| {
            BuilderError::ExecutionFailed(format!("failed to load host idmap: {error}"))
        })?;
        let rootfs_root_dir = validate_rootfs(rootfs, idmap.as_ref()).map_err(map_error)?;

        let extra_inputs =
            collect_extra_inputs(&SANDBOX_SPEC, "Sandbox", &inputs).map_err(map_error)?;
        validate_step_interpolations(&config.steps, &extra_inputs).map_err(map_error)?;

        let output_path = cx.temp_dir.join(OUTPUT_DIR_NAME);
        fsutil::recreate_empty_dir_force(&output_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        let config_path = cx.temp_dir.join(CONFIG_DIR_NAME);
        fsutil::recreate_empty_dir_force(&config_path)
            .map_err(map_fsutil_error)
            .map_err(map_error)?;
        write_script_config(&config_path, config.script_config.as_ref()).map_err(map_error)?;

        let sandbox_inputs = extra_inputs
            .iter()
            .map(|(name, input)| build_sandbox_input(name, input))
            .collect::<BResult<Vec<_>>>()
            .map_err(map_error)?;

        let sandbox_steps = config
            .steps
            .iter()
            .map(|step| build_sandbox_step(step, &extra_inputs, cx))
            .collect::<BResult<Vec<_>>>()
            .map_err(map_error)?;

        let workspace = cx.temp_dir.join("runtime");
        std::fs::create_dir_all(&workspace)
            .map_err(|error| {
                SandboxError::FsFailed(format!(
                    "failed to create sandbox runtime workspace '{}': {error}",
                    workspace.display()
                ))
            })
            .map_err(map_error)?;

        let sandbox_config = SandboxBuildConfig {
            rootfs: rootfs_root_dir,
            out_dir: output_path.clone(),
            config_dir: config_path,
            workspace,
            inputs: sandbox_inputs,
            steps: sandbox_steps,
        };

        cx.log_event(
            BuildLogLevel::Info,
            "sandbox-prepare",
            format!(
                "prepared readonly rootfs, {} input mount(s), build dir, and config dir",
                extra_inputs.len()
            ),
        );

        let outcome = run_sandbox_build(sandbox_config, idmap.as_ref()).map_err(|error| {
            BuilderError::ExecutionFailed(format!("sandbox build failed: {error}"))
        })?;
        write_build_report(cx, &outcome);

        let staged_path =
            stage_fs_tree_output(cx, &output_path, &outcome.manifest).map_err(map_error)?;

        Ok(StagedBuildResult {
            staged_path,
            object_hash: Some(outcome.object_hash),
        })
    }
}

/// Convert the raw sandbox output directory into the canonical fs-tree shape.
///
/// The runtime scans and hashes the output before this function is called. This
/// staging step preserves that manifest and moves the actual output tree under
/// `root/`, which is the store representation used for fs-tree objects.
fn stage_fs_tree_output(
    cx: &BuildContext,
    output_path: &Path,
    manifest: &mbuild_core::FsTreeManifest,
) -> BResult<PathBuf> {
    let staged_path = cx.temp_dir.join(FS_TREE_OBJECT_DIR_NAME);
    fsutil::recreate_empty_dir_force(&staged_path).map_err(map_fsutil_error)?;
    let manifest_path = staged_path.join("manifest.jsonl");
    manifest.write_canonical(&manifest_path).map_err(|error| {
        SandboxError::BuildFailed(format!(
            "failed to write sandbox fs-tree manifest '{}': {error}",
            manifest_path.display()
        ))
    })?;
    let root_path = staged_path.join("root");
    std::fs::rename(output_path, &root_path).map_err(|error| {
        SandboxError::FsFailed(format!(
            "failed to stage sandbox output '{}' -> '{}': {error}",
            output_path.display(),
            root_path.display()
        ))
    })?;
    Ok(staged_path)
}

/// Build one runtime step config consumed by `mbuild-runtime`.
fn build_sandbox_step(
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
    cx: &BuildContext,
) -> BResult<SandboxStep> {
    let cwd = PathBuf::from(resolve_step_cwd(step, inputs)?);
    let argv = resolve_step_argv(step, inputs)?;
    let env = resolve_step_env(step, inputs)?
        .into_iter()
        .collect::<HashMap<_, _>>();
    let logs = cx.temp_dir.join("step-logs");
    std::fs::create_dir_all(&logs).map_err(|error| {
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

    Ok(SandboxStep {
        name: step.name.clone(),
        run_as: match step.run_as {
            StepUser::BuildUser => SandboxRunAs::BuildUser,
            StepUser::Root => SandboxRunAs::Root,
        },
        cwd,
        argv,
        env,
        stdout_path,
        stderr_path,
    })
}

/// Allocate the host log file path reported for a step stream.
///
/// Build contexts normally allocate stable raw log paths. The fallback keeps
/// unit tests and minimal contexts functional without changing report shape.
fn allocate_step_log_path(cx: &BuildContext, label: &str, fallback: PathBuf) -> BResult<PathBuf> {
    let path = match cx.allocate_raw_log_path(label) {
        Ok(path) => path,
        Err(_) => fallback,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
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
///
/// Step-level stdout/stderr logs are written directly by the runner. This
/// report ties those logs to the output hash and canonical manifest for easier
/// inspection after a successful sandbox build.
fn write_build_report(cx: &BuildContext, outcome: &mbuild_runtime::SandboxBuildOutcome) {
    let steps = outcome
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
    let manifest_jsonl = outcome
        .manifest
        .to_canonical_bytes()
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let report = serde_json::json!({
        "object_hash": outcome.object_hash.to_string(),
        "manifest_jsonl": manifest_jsonl,
        "steps": steps,
    });
    if let Ok(text) = serde_json::to_string_pretty(&report) {
        let log_path = cx.write_raw_log("sandbox-result", &text);
        cx.log_event_with_details(
            BuildLogLevel::Info,
            "sandbox-result",
            format!("sandbox output hash {}", outcome.object_hash),
            Some(outcome.object_hash),
            log_path,
            Map::new(),
        );
    }
}

/// Validate the full recipe-facing config before host paths are prepared.
fn validate_sandbox_config(config: &SandboxConfig) -> BResult<()> {
    validate_script_config(config.script_config.as_ref())?;
    validate_steps(&config.steps)
}

/// Ensure the required `rootfs` input is a valid fs-tree object.
fn validate_rootfs(
    rootfs: &BuilderInputObject,
    owner_map: &impl FsTreeOwnerMap,
) -> BResult<PathBuf> {
    validate_fs_tree_object(&rootfs.object_path, owner_map)
        .map(|validated| validated.paths.root_dir)
        .map_err(|error| {
            SandboxError::InputResolutionFailed(format!(
                "rootfs input must be a valid fs-tree object '{}': {error}",
                rootfs.object_path.display()
            ))
        })
}

/// Validate step shape without resolving interpolation.
fn validate_steps(steps: &[BuildStep]) -> BResult<()> {
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
fn validate_step_env(env: &Map<String, Value>, path: &str) -> BResult<()> {
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
fn validate_env_key(key: &str, path: &str) -> BResult<()> {
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
fn validate_input_name(name: &str) -> BResult<()> {
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
    format!("{INPUT_MOUNT_ROOT}/{name}")
}

/// Lower one extra input to the runtime mount shape.
///
/// Extra inputs are mounted as complete store objects. They do not interpret
/// fs-tree layout files such as `manifest.jsonl` or `root/`.
fn build_sandbox_input(name: &str, input: &BuilderInputObject) -> BResult<SandboxInput> {
    let host_path = input.object_path.clone();
    if !host_path.is_dir() && !host_path.is_file() {
        return Err(SandboxError::InputResolutionFailed(format!(
            "sandbox input must resolve to a file or directory: {}",
            host_path.display()
        )));
    }
    Ok(SandboxInput {
        name: name.to_string(),
        host_path,
        mount_path: PathBuf::from(input_mount_path(name)),
    })
}

/// Collect and validate all extra inputs accepted by the `Sandbox` spec.
///
/// The `rootfs` input is handled separately. Extra inputs become readonly
/// mounts and interpolation variables. Reserved names are rejected because they
/// would shadow built-in variables.
fn collect_extra_inputs(
    spec: &BuilderSpec,
    builder_name: &str,
    inputs: &BuilderInputs,
) -> BResult<Vec<(String, BuilderInputObject)>> {
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
///
/// `@@{name}` is an escape that produces a literal `@{name}`. The renderer is
/// byte-index based, but it advances by UTF-8 char width for non-syntax text so
/// non-ASCII literals remain valid even though interpolation names are ASCII.
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
fn validate_interpolation_name(key: &str, value: &str, escaped: bool) -> BResult<()> {
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
                SandboxError::InvalidConfig(format!("unknown interpolation variable '@{{{key}}}'"))
            }),
    }
}

/// Resolve and validate a step working directory.
fn resolve_step_cwd(step: &BuildStep, inputs: &[(String, BuilderInputObject)]) -> BResult<String> {
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
    inputs: &[(String, BuilderInputObject)],
) -> BResult<Vec<String>> {
    step.argv
        .iter()
        .map(|arg| interpolate_step_string(arg, inputs))
        .collect()
}

/// Resolve all environment values for a step.
fn resolve_step_env(
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<Vec<(String, String)>> {
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
///
/// The builder calls this before creating host runtime directories so config
/// errors are reported as recipe errors rather than partial execution failures.
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

/// Convert fsutil errors into sandbox-local filesystem errors.
fn map_fsutil_error(error: fsutil::FsUtilError) -> SandboxError {
    SandboxError::FsFailed(error.to_string())
}

/// Map sandbox-local errors to the recipe runtime error categories.
fn map_error(error: SandboxError) -> BuilderError {
    match error {
        SandboxError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        SandboxError::InputResolutionFailed(message)
        | SandboxError::BuildFailed(message)
        | SandboxError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}

/// Validate `script_config` before materializing it on disk.
fn validate_script_config(value: Option<&Value>) -> BResult<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => validate_script_config_node(value, "<root>"),
    }
}

/// Recursively validate one `script_config` node.
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
        _ => Err(SandboxError::InvalidConfig(format!(
            "script_config supports only records, arrays, and string leaves; invalid value at {path}"
        ))),
    }
}

/// Validate a `script_config` object key before using it as a path segment.
fn validate_script_config_key(key: &str, path: &str) -> BResult<()> {
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
///
/// Records and arrays become directories. String leaves become files. Array
/// entries use zero-padded numeric filenames so lexical order preserves array
/// order.
fn write_script_config(root: &Path, value: Option<&Value>) -> BResult<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => write_script_config_node(root, value, "<root>"),
    }
}

/// Recursively write one `script_config` node to the host filesystem.
fn write_script_config_node(path: &Path, value: &Value, debug_path: &str) -> BResult<()> {
    match value {
        Value::String(contents) => fs::write(path, contents).map_err(|error| {
            SandboxError::FsFailed(format!(
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
        _ => Err(SandboxError::InvalidConfig(format!(
            "script_config supports only records, arrays, and string leaves; invalid value at {debug_path}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{
        Builder, BuilderInputs, FsTreeEntry, FsTreeManifest, FsTreeObjectError, FsTreeOwnerMap,
        IdentityFsTreeOwnerMap,
    };
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::tempdir;

    #[test]
    fn sandbox_spec_uses_rootfs_required_input() {
        assert_eq!(SANDBOX_SPEC.tag, "Sandbox");
        assert_eq!(SANDBOX_SPEC.required_inputs, &["rootfs"]);
        assert!(SANDBOX_SPEC.allow_extra_inputs);
    }

    #[test]
    fn sandbox_builder_rejects_missing_rootfs() {
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.state_dir).unwrap();
        std::fs::create_dir_all(&cx.temp_dir).unwrap();

        let config = json!({
            "steps": [{
                "name": "build",
                "run_as": "build-user",
                "cwd": "/",
                "argv": ["true"]
            }]
        });

        let error = SandboxBuilder
            .build_erased(config, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("rootfs"));
    }

    #[test]
    fn sandbox_builder_rejects_install_config() {
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.state_dir).unwrap();
        std::fs::create_dir_all(&cx.temp_dir).unwrap();

        let config = json!({
            "steps": [{
                "name": "build",
                "run_as": "build-user",
                "cwd": "/",
                "argv": ["true"]
            }],
            "install": {
                "rules": [{
                    "path": "**",
                    "attrs": {
                        "uid": 0,
                        "gid": 0,
                        "directory_mode": 493,
                        "regular_file_mode": 420,
                        "executable_file_mode": 493,
                        "symlink_mode": 511
                    }
                }]
            }
        });

        let error = SandboxBuilder
            .build_erased(config, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("unknown field `install`"));
    }

    #[test]
    fn stage_fs_tree_output_writes_manifest_and_moves_raw_output_to_root() {
        let temp = tempdir().unwrap();
        let cx = BuildContext::with_noop_logger(temp.path().join("state"), temp.path().join("tmp"));
        std::fs::create_dir_all(&cx.temp_dir).unwrap();
        let output = cx.temp_dir.join("out");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("file"), "contents").unwrap();
        let manifest = mbuild_core::FsTreeManifest::from_entries(vec![
            mbuild_core::FsTreeEntry::directory("", 0, 0, 0o755),
            mbuild_core::FsTreeEntry::file("file", 0, 0, 0o644),
        ])
        .unwrap();

        let staged = stage_fs_tree_output(&cx, &output, &manifest).unwrap();

        assert!(staged.join("manifest.jsonl").is_file());
        assert_eq!(
            mbuild_core::FsTreeManifest::read_canonical(&staged.join("manifest.jsonl")).unwrap(),
            manifest
        );
        assert_eq!(
            std::fs::read_to_string(staged.join("root").join("file")).unwrap(),
            "contents"
        );
        assert!(!output.exists());
    }

    #[derive(Debug, Clone, Copy)]
    struct ConstantOwnerMap {
        uid: u32,
        gid: u32,
    }

    impl FsTreeOwnerMap for ConstantOwnerMap {
        fn physical_uid(&self, _logical_uid: u32) -> Result<u32, FsTreeObjectError> {
            Ok(self.uid)
        }

        fn physical_gid(&self, _logical_gid: u32) -> Result<u32, FsTreeObjectError> {
            Ok(self.gid)
        }
    }

    fn input_object(object_path: PathBuf) -> BuilderInputObject {
        BuilderInputObject {
            object_path,
            object_hash: "0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap(),
        }
    }

    #[cfg(unix)]
    fn write_minimal_fs_tree_object(object: &Path) -> ConstantOwnerMap {
        std::fs::create_dir(object).unwrap();
        let root = object.join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = std::fs::metadata(&root).unwrap();
        let manifest =
            FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap();
        manifest
            .write_canonical(&object.join("manifest.jsonl"))
            .unwrap();
        ConstantOwnerMap {
            uid: metadata.uid(),
            gid: metadata.gid(),
        }
    }

    fn minimal_step(name: &str) -> BuildStep {
        BuildStep {
            name: name.to_string(),
            run_as: StepUser::BuildUser,
            cwd: "/".to_string(),
            argv: vec!["true".to_string()],
            env: Map::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_rootfs_accepts_valid_fs_tree_and_returns_root_dir() {
        let temp = tempdir().unwrap();
        let object = temp.path().join("object");
        let owner = write_minimal_fs_tree_object(&object);

        assert_eq!(
            validate_rootfs(&input_object(object.clone()), &owner).unwrap(),
            object.join("root")
        );
    }

    #[test]
    fn validate_rootfs_rejects_plain_directory() {
        let temp = tempdir().unwrap();
        let object = temp.path().join("object");
        std::fs::create_dir_all(&object).unwrap();

        let error = validate_rootfs(&input_object(object.clone()), &IdentityFsTreeOwnerMap)
            .unwrap_err()
            .to_string();

        assert!(error.contains("rootfs input must be a valid fs-tree object"));
        assert!(error.contains(&object.display().to_string()));
    }

    #[test]
    fn sandbox_input_mounts_fs_tree_shaped_object_directory() {
        let temp = tempdir().unwrap();
        let object = temp.path().join("object");
        std::fs::create_dir_all(object.join("root")).unwrap();
        std::fs::write(object.join("manifest.jsonl"), "").unwrap();

        let input = build_sandbox_input("source", &input_object(object.clone())).unwrap();

        assert_eq!(input.name, "source");
        assert_eq!(input.host_path, object);
        assert_eq!(input.mount_path, PathBuf::from("/__mbuild/inputs/source"));
    }

    #[test]
    fn sandbox_input_accepts_plain_file_and_directory_objects() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("file-object");
        let dir = temp.path().join("dir-object");
        std::fs::write(&file, "payload").unwrap();
        std::fs::create_dir(&dir).unwrap();

        assert_eq!(
            build_sandbox_input("file", &input_object(file.clone()))
                .unwrap()
                .host_path,
            file
        );
        assert_eq!(
            build_sandbox_input("dir", &input_object(dir.clone()))
                .unwrap()
                .host_path,
            dir
        );
    }

    #[test]
    fn validate_steps_rejects_duplicate_step_names() {
        let error = validate_steps(&[minimal_step("build"), minimal_step("build")])
            .unwrap_err()
            .to_string();

        assert!(error.contains("steps[1].name 'build' duplicates steps[0].name"));
    }

    #[test]
    fn validate_steps_rejects_sanitized_log_name_collisions() {
        let error = validate_steps(&[minimal_step("a/b"), minimal_step("a_b")])
            .unwrap_err()
            .to_string();

        assert!(error.contains("steps[1].name 'a_b' collides with steps[0].name 'a/b'"));
        assert!(error.contains("a_b"));
    }

    #[test]
    fn validate_steps_accepts_distinct_non_colliding_names() {
        validate_steps(&[minimal_step("a/b"), minimal_step("a-b")]).unwrap();
    }

    #[test]
    fn collect_extra_inputs_rejects_reserved_and_invalid_names() {
        let temp = tempdir().unwrap();
        let mut reserved = BuilderInputs::empty();
        reserved.insert("build", input_object(temp.path().join("build")));
        let error = collect_extra_inputs(&SANDBOX_SPEC, "Sandbox", &reserved)
            .unwrap_err()
            .to_string();
        assert!(error.contains("input name 'build' conflicts with a reserved Sandbox"));

        let mut invalid = BuilderInputs::empty();
        invalid.insert("bad-name", input_object(temp.path().join("bad-name")));
        let error = collect_extra_inputs(&SANDBOX_SPEC, "Sandbox", &invalid)
            .unwrap_err()
            .to_string();
        assert!(error.contains("input name 'bad-name' must contain only ASCII"));
    }

    #[test]
    fn interpolation_rejects_malformed_and_unknown_variables() {
        let error = interpolate_step_string("@{missing", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("unterminated interpolation"));

        let error = interpolate_step_string("@{bad-name}", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid interpolation variable '@{bad-name}'"));

        let error = interpolate_step_string("@{missing}", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown interpolation variable '@{missing}'"));
    }

    #[test]
    fn env_validation_uses_env_specific_errors() {
        let mut env = Map::new();
        env.insert("bad/key".to_string(), Value::String("value".to_string()));

        let error = validate_step_env(&env, "steps[0].env")
            .unwrap_err()
            .to_string();

        assert!(error.contains("env key 'bad/key' at steps[0].env is invalid"));
        assert!(!error.contains("script_config key"));
    }

    #[test]
    fn env_validation_rejects_non_string_values() {
        let mut env = Map::new();
        env.insert("CC".to_string(), Value::from(1));

        let error = validate_step_env(&env, "steps[0].env")
            .unwrap_err()
            .to_string();

        assert!(error.contains("steps[0].env.CC must be a string"));
    }

    #[test]
    fn script_config_validation_rejects_invalid_key_and_value() {
        let error = validate_script_config(Some(&json!({ "bad/key": "value" })))
            .unwrap_err()
            .to_string();
        assert!(error.contains("script_config key 'bad/key' at <root> is invalid"));

        let error = validate_script_config(Some(&json!(true)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("script_config supports only records, arrays, and string leaves"));
    }

    fn step() -> BuildStep {
        BuildStep {
            name: "build".to_string(),
            run_as: StepUser::BuildUser,
            cwd: "@{source}".to_string(),
            argv: vec!["@{script}".to_string(), "--flag".to_string()],
            env: Map::new(),
        }
    }

    #[test]
    fn step_interpolation_resolves_extra_inputs() {
        let temp = tempdir().unwrap();
        let inputs = vec![
            (
                "script".to_string(),
                BuilderInputObject {
                    object_path: temp.path().join("script"),
                    object_hash: "0000000000000000000000000000000000000000000000000000000000000000"
                        .parse()
                        .unwrap(),
                },
            ),
            (
                "source".to_string(),
                BuilderInputObject {
                    object_path: temp.path().join("source"),
                    object_hash: "1111111111111111111111111111111111111111111111111111111111111111"
                        .parse()
                        .unwrap(),
                },
            ),
        ];

        assert_eq!(
            resolve_step_cwd(&step(), &inputs).unwrap(),
            "/__mbuild/inputs/source"
        );
        assert_eq!(
            resolve_step_argv(&step(), &inputs).unwrap(),
            vec!["/__mbuild/inputs/script", "--flag"]
        );
    }

    #[test]
    fn script_config_materializes_tree() {
        let temp = tempdir().unwrap();
        write_script_config(
            temp.path(),
            Some(&json!({
                "args": ["--disable-nls"],
                "env": { "CC": "gcc" },
            })),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("args").join("00000000")).unwrap(),
            "--disable-nls"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("env").join("CC")).unwrap(),
            "gcc"
        );
    }
}
