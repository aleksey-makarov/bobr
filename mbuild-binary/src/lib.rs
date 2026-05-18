use mbuild_core::{BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec, fsutil};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
use std::path::Path;

mod sandbox;

pub use sandbox::SandboxBuilder;

pub(crate) const OUTPUT_DIR_NAME: &str = "out";
pub(crate) const CONFIG_DIR_NAME: &str = "config";
pub(crate) const INPUT_MOUNT_ROOT: &str = "/__mbuild/inputs";
pub(crate) const CONFIG_MOUNT_PATH: &str = "/__mbuild/config";
pub(crate) const BUILD_DIR_MOUNT_PATH: &str = "/__mbuild/build";
pub(crate) const OUT_DIR_MOUNT_PATH: &str = "/__mbuild/out";

#[derive(Debug)]
pub(crate) enum SandboxError {
    InvalidConfig(String),
    InputResolutionFailed(String),
    BuildFailed(String),
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

pub(crate) type BResult<T> = Result<T, SandboxError>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StepUser {
    BuildUser,
    Root,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildStep {
    pub(crate) name: String,
    pub(crate) run_as: StepUser,
    pub(crate) cwd: String,
    pub(crate) argv: Vec<String>,
    #[serde(default)]
    pub(crate) env: Map<String, Value>,
}

pub(crate) fn validate_steps(steps: &[BuildStep]) -> BResult<()> {
    if steps.is_empty() {
        return Err(SandboxError::InvalidConfig(
            "steps must contain at least one step".to_string(),
        ));
    }

    for (index, step) in steps.iter().enumerate() {
        if step.name.trim().is_empty() {
            return Err(SandboxError::InvalidConfig(format!(
                "steps[{index}].name must not be empty"
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

fn validate_step_env(env: &Map<String, Value>, path: &str) -> BResult<()> {
    for (key, value) in env {
        validate_script_config_key(key, path)?;
        if !matches!(value, Value::String(_)) {
            return Err(SandboxError::InvalidConfig(format!(
                "{path}.{key} must be a string"
            )));
        }
    }
    Ok(())
}

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

pub(crate) fn input_mount_path(name: &str) -> String {
    format!("{INPUT_MOUNT_ROOT}/{name}")
}

pub(crate) fn collect_named_inputs(
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

pub(crate) fn resolve_step_cwd(
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<String> {
    let cwd = interpolate_step_string(&step.cwd, inputs)?;
    if cwd.is_empty() || !cwd.starts_with('/') {
        return Err(SandboxError::InvalidConfig(format!(
            "step '{}' resolved cwd must be an absolute path, got '{}'",
            step.name, cwd
        )));
    }
    Ok(cwd)
}

pub(crate) fn resolve_step_argv(
    step: &BuildStep,
    inputs: &[(String, BuilderInputObject)],
) -> BResult<Vec<String>> {
    step.argv
        .iter()
        .map(|arg| interpolate_step_string(arg, inputs))
        .collect()
}

pub(crate) fn resolve_step_env(
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

pub(crate) fn validate_step_interpolations(
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

pub(crate) fn map_fsutil_error(error: fsutil::FsUtilError) -> SandboxError {
    SandboxError::FsFailed(error.to_string())
}

pub(crate) fn map_error(error: SandboxError) -> BuilderError {
    match error {
        SandboxError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        SandboxError::InputResolutionFailed(message)
        | SandboxError::BuildFailed(message)
        | SandboxError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}

pub(crate) fn validate_script_config(value: Option<&Value>) -> BResult<()> {
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
        _ => Err(SandboxError::InvalidConfig(format!(
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
        return Err(SandboxError::InvalidConfig(format!(
            "script_config key '{}' at {} is invalid; allowed chars: [A-Za-z0-9._-]",
            key, path
        )));
    }
    Ok(())
}

pub(crate) fn write_script_config(root: &Path, value: Option<&Value>) -> BResult<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => write_script_config_node(root, value, "<root>"),
    }
}

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
    use serde_json::json;
    use tempfile::tempdir;

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
    fn step_interpolation_resolves_named_inputs() {
        let temp = tempdir().unwrap();
        let inputs = vec![
            (
                "script".to_string(),
                BuilderInputObject {
                    object_path: temp.path().join("script"),
                },
            ),
            (
                "source".to_string(),
                BuilderInputObject {
                    object_path: temp.path().join("source"),
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
