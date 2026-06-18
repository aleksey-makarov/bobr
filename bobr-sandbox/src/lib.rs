//! New sandbox builder prototype backed by `bobr-runtime`.
//!
//! This crate intentionally does not reuse or refactor the existing
//! `mbuild-sandbox` implementation. It provides a fresh `SandboxNew` builder
//! surface and a stub runtime function that will grow into the real sandbox
//! launcher.

use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use mbuild_builder::{
    BuildContext, BuilderInputs, BuilderRegistry, InputSlot, InputSpec, StagedBuildResult,
    TypedBuilder,
};
use mbuild_core::{BuildLogLevel, BuilderError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

/// Builder implementation registered for recipe nodes tagged `SandboxNew`.
pub struct SandboxNewBuilder;

/// Static `SandboxNew` builder class used by explicit registries.
pub static SANDBOX_NEW_BUILDER: SandboxNewBuilder = SandboxNewBuilder;

/// Registers the `SandboxNew` builder into an explicit builder registry.
pub fn register_builders(registry: &mut BuilderRegistry) -> Result<(), String> {
    registry.register(&SANDBOX_NEW_BUILDER)
}

/// Return runtime functions supported by `bobr-sandbox`.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![bobr_runtime::runtime_ns::NsFunction::new(SandboxFunction)]
}

/// Recipe-facing `SandboxNew` builder config.
///
/// This shape intentionally matches the existing `Sandbox` config. The input
/// contract differs: `rootfs` is a materialized fs-tree v2 root.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxNewConfig {
    /// Optional tree of config files exposed to build steps.
    #[serde(default)]
    script_config: Option<Value>,
    /// Ordered command steps to execute inside the sandbox.
    steps: Vec<BuildStep>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum StepUser {
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

static SANDBOX_NEW_SPEC: InputSpec = InputSpec {
    required_inputs: &[InputSlot::fs_tree_root("rootfs")],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for SandboxNewBuilder {
    type Config = SandboxNewConfig;

    fn tag(&self) -> &'static str {
        "SandboxNew"
    }

    fn spec(&self) -> &'static InputSpec {
        &SANDBOX_NEW_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_sandbox_new(config, inputs, cx)
    }
}

fn build_sandbox_new(
    config: SandboxNewConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<StagedBuildResult, BuilderError> {
    validate_sandbox_config(&config)?;
    let rootfs = inputs.required("rootfs")?.path.clone();
    let extra_inputs = collect_extra_inputs(&inputs)?;
    let input = SandboxInput {
        rootfs,
        step_count: config.steps.len(),
        has_script_config: config.script_config.is_some(),
        extra_inputs,
    };

    cx.log_event(
        BuildLogLevel::Info,
        "sandbox",
        format!(
            "running SandboxNew stub with {} step(s) and {} extra input(s)",
            input.step_count,
            input.extra_inputs.len()
        ),
    );

    let output = cx
        .runtime()
        .run(&SandboxFunction, input)
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let output_path = cx.temp_dir.join("sandbox-new-stub.json");
    let file = std::fs::File::create(&output_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create SandboxNew stub output '{}': {error}",
            output_path.display()
        ))
    })?;
    serde_json::to_writer_pretty(file, &output).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to write SandboxNew stub output '{}': {error}",
            output_path.display()
        ))
    })?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: None,
    })
}

#[derive(Debug)]
enum SandboxConfigError {
    InvalidConfig(String),
}

impl fmt::Display for SandboxConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => f.write_str(message),
        }
    }
}

fn map_config_error(error: SandboxConfigError) -> BuilderError {
    BuilderError::InvalidRecipe(error.to_string())
}

fn validate_sandbox_config(config: &SandboxNewConfig) -> Result<(), BuilderError> {
    validate_script_config(config.script_config.as_ref()).map_err(map_config_error)?;
    validate_steps(&config.steps).map_err(map_config_error)
}

fn validate_script_config(value: Option<&Value>) -> Result<(), SandboxConfigError> {
    if let Some(value) = value {
        validate_script_config_value(value, "script_config")?;
    }
    Ok(())
}

fn validate_script_config_value(value: &Value, path: &str) -> Result<(), SandboxConfigError> {
    match value {
        Value::String(_) => Ok(()),
        Value::Object(object) => {
            for (key, value) in object {
                if key.is_empty() || key == "." || key == ".." || key.contains('/') {
                    return Err(SandboxConfigError::InvalidConfig(format!(
                        "{path}: config key '{key}' is invalid"
                    )));
                }
                validate_script_config_value(value, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        _ => Err(SandboxConfigError::InvalidConfig(format!(
            "{path} must contain only nested objects and string leaves"
        ))),
    }
}

fn validate_steps(steps: &[BuildStep]) -> Result<(), SandboxConfigError> {
    if steps.is_empty() {
        return Err(SandboxConfigError::InvalidConfig(
            "steps must contain at least one step".to_string(),
        ));
    }

    let mut seen_names = HashMap::new();
    let mut seen_log_names = HashMap::new();
    for (index, step) in steps.iter().enumerate() {
        if step.name.trim().is_empty() {
            return Err(SandboxConfigError::InvalidConfig(format!(
                "steps[{index}].name must not be empty"
            )));
        }
        if let Some(previous) = seen_names.insert(step.name.as_str(), index) {
            return Err(SandboxConfigError::InvalidConfig(format!(
                "steps[{index}].name '{}' duplicates steps[{previous}].name",
                step.name
            )));
        }
        let log_name = sanitize_log_name(&step.name);
        if let Some(previous) = seen_log_names.insert(log_name.clone(), index) {
            return Err(SandboxConfigError::InvalidConfig(format!(
                "steps[{index}].name '{}' collides with steps[{previous}].name '{}' after log-name sanitization ('{log_name}')",
                step.name, steps[previous].name
            )));
        }
        if step.cwd.trim().is_empty() {
            return Err(SandboxConfigError::InvalidConfig(format!(
                "steps[{index}].cwd must not be empty"
            )));
        }
        if step.argv.is_empty() {
            return Err(SandboxConfigError::InvalidConfig(format!(
                "steps[{index}].argv must not be empty"
            )));
        }
        for (arg_index, arg) in step.argv.iter().enumerate() {
            if arg.is_empty() {
                return Err(SandboxConfigError::InvalidConfig(format!(
                    "steps[{index}].argv[{arg_index}] must not be empty"
                )));
            }
        }
        validate_step_env(&step.env, &format!("steps[{index}].env"))?;
    }

    Ok(())
}

fn validate_step_env(env: &Map<String, Value>, path: &str) -> Result<(), SandboxConfigError> {
    for (key, value) in env {
        validate_env_key(key, path)?;
        if !matches!(value, Value::String(_)) {
            return Err(SandboxConfigError::InvalidConfig(format!(
                "{path}.{key} must be a string"
            )));
        }
    }
    Ok(())
}

fn validate_env_key(key: &str, path: &str) -> Result<(), SandboxConfigError> {
    if key.is_empty()
        || key == "."
        || key == ".."
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(SandboxConfigError::InvalidConfig(format!(
            "env key '{}' at {} is invalid; allowed chars: [A-Za-z0-9._-]",
            key, path
        )));
    }
    Ok(())
}

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

fn collect_extra_inputs(inputs: &BuilderInputs) -> Result<Vec<SandboxRuntimeInput>, BuilderError> {
    let mut extra_inputs = Vec::new();
    for (name, input) in inputs.extras(&SANDBOX_NEW_SPEC) {
        validate_input_name(name)?;
        if matches!(name, "build" | "out" | "config") {
            return Err(BuilderError::InvalidRecipe(format!(
                "input name '{name}' conflicts with a reserved SandboxNew interpolation variable"
            )));
        }
        extra_inputs.push(SandboxRuntimeInput {
            name: name.to_string(),
            path: input.path.clone(),
        });
    }
    Ok(extra_inputs)
}

fn validate_input_name(name: &str) -> Result<(), BuilderError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(BuilderError::InvalidRecipe(
            "input name must not be empty".to_string(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(BuilderError::InvalidRecipe(format!(
            "input name '{name}' must start with an ASCII letter or underscore"
        )));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(BuilderError::InvalidRecipe(format!(
            "input name '{name}' must contain only ASCII letters, digits, and underscores"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct SandboxFunction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxInput {
    rootfs: PathBuf,
    step_count: usize,
    has_script_config: bool,
    extra_inputs: Vec<SandboxRuntimeInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxRuntimeInput {
    name: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxOutput {
    rootfs: PathBuf,
    step_count: usize,
    extra_input_count: usize,
    message: String,
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
                "SandboxNew rootfs must be a directory: '{}'",
                input.rootfs.display()
            )));
        }
        Ok(SandboxOutput {
            rootfs: input.rootfs,
            step_count: input.step_count,
            extra_input_count: input.extra_inputs.len(),
            message: if input.has_script_config {
                "SandboxNew stub accepted config with script_config".to_string()
            } else {
                "SandboxNew stub accepted config".to_string()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_builder::{Builder, BuilderInputPath};
    use serde_json::json;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn valid_config() -> SandboxNewConfig {
        SandboxNewConfig {
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
        BuilderInputs::new(BTreeMap::from([(
            "rootfs".to_string(),
            BuilderInputPath { path: rootfs },
        )]))
    }

    #[test]
    fn spec_requires_fs_tree_root_rootfs_and_allows_extra_inputs() {
        assert_eq!(TypedBuilder::tag(&SandboxNewBuilder), "SandboxNew");
        assert_eq!(SANDBOX_NEW_SPEC.required_inputs.len(), 1);
        assert_eq!(
            SANDBOX_NEW_SPEC.required_inputs[0],
            InputSlot::fs_tree_root("rootfs")
        );
        assert!(SANDBOX_NEW_SPEC.optional_inputs.is_empty());
        assert!(SANDBOX_NEW_SPEC.allow_extra_inputs);
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

        let error = SandboxNewBuilder
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
        std::fs::create_dir(&rootfs).unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = SandboxNewBuilder
            .build_typed(
                SandboxNewConfig {
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
    fn build_calls_runtime_stub_and_stages_json_output() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let source = temp.path().join("source");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::create_dir(&source).unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));
        std::fs::create_dir(&cx.temp_dir).unwrap();

        let result = SandboxNewBuilder
            .build_typed(
                valid_config(),
                BuilderInputs::new(BTreeMap::from([
                    (
                        "rootfs".to_string(),
                        BuilderInputPath {
                            path: rootfs.clone(),
                        },
                    ),
                    ("source".to_string(), BuilderInputPath { path: source }),
                ])),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            result.staged_path,
            temp.path().join("tmp").join("sandbox-new-stub.json")
        );
        assert!(result.object_hash.is_none());
        let output: SandboxOutput =
            serde_json::from_slice(&std::fs::read(&result.staged_path).unwrap()).unwrap();
        assert_eq!(output.rootfs, rootfs);
        assert_eq!(output.step_count, 1);
        assert_eq!(output.extra_input_count, 1);
        assert_eq!(output.message, "SandboxNew stub accepted config");
    }

    #[test]
    fn build_reports_missing_rootfs_directory() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("missing-rootfs");
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = SandboxNewBuilder
            .build_typed(valid_config(), inputs(rootfs), &mut cx)
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
        assert!(error.to_string().contains("rootfs must be a directory"));
    }
}
