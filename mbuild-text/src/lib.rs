use mbuild_core::{
    BuildContext, BuilderError, BuilderSpec, ProducerInfo, ResolvedInputs, StagedBuildResult,
    TypedBuilder, fsutil,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const BUILDER_NAME: &str = "text";

#[derive(Debug)]
enum TextError {
    InvalidConfig(String),
}

impl TextError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(m) => m,
        }
    }
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

type TResult<T> = Result<T, TextError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextConfig {
    kind: String,
    source: String,
}

pub struct TextBuilder;

static TEXT_SPEC: BuilderSpec = BuilderSpec {
    tag: "Text",
    inputs: &[],
};

impl TypedBuilder for TextBuilder {
    type Config = TextConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &TEXT_SPEC
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
                "Text builder does not accept input objects".to_string(),
            ));
        }

        fs::create_dir_all(&cx.temp_root).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to create text temp directory '{}': {error}",
                cx.temp_root.display()
            ))
        })?;

        let now_nanos = fsutil::current_epoch_nanos()
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        let tmp_path = cx.temp_root.join(format!("text-{now_nanos}.obj"));

        if tmp_path.exists() {
            fs::remove_file(&tmp_path).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to remove previous temporary file '{}': {error}",
                    tmp_path.display()
                ))
            })?;
        }

        fs::write(&tmp_path, &config.source).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to write staged text output '{}': {error}",
                tmp_path.display()
            ))
        })?;

        #[cfg(unix)]
        if config.kind == "build-script" {
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(&tmp_path, perms).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to set executable mode on staged build-script '{}': {error}",
                    tmp_path.display()
                ))
            })?;
        }

        let mut attrs = Map::new();
        attrs.insert(
            "source_bytes".to_string(),
            Value::from(config.source.len() as u64),
        );

        Ok(StagedBuildResult {
            kind: config.kind,
            producer: ProducerInfo {
                builder: BUILDER_NAME.to_string(),
            },
            input_object_hashes: vec![],
            attrs,
            staged_path: tmp_path,
        })
    }
}

fn validate_config(config: &TextConfig) -> TResult<()> {
    if config.kind.is_empty() {
        return Err(TextError::InvalidConfig("kind must not be empty".to_string()));
    }
    Ok(())
}

fn map_error(error: TextError) -> BuilderError {
    match error {
        TextError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
    }
}
