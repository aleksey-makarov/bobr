use crate::BuilderError;
use crate::cas::{BuildKey, MetaHash};
use fsobj_hash::ObjectHash;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
pub struct BuildRequest {
    pub meta: BuildMeta,
    pub build: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildMeta {
    pub name: String,
    #[serde(default)]
    pub extra: Map<String, Value>,
}

#[derive(Debug)]
pub struct BuilderSpec {
    pub tag: &'static str,
    pub inputs: &'static [InputSlot],
}

#[derive(Debug)]
pub struct InputSlot {
    pub name: &'static str,
    pub arity: InputArity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputArity {
    One,
    Optional,
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderInputObject {
    pub object_path: PathBuf,
    pub meta: Map<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct BuilderInputs {
    slots: BTreeMap<String, BuilderInputValue>,
}

impl BuilderInputs {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new(slots: BTreeMap<String, BuilderInputValue>) -> Self {
        Self { slots }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn insert(&mut self, name: impl Into<String>, value: BuilderInputValue) {
        self.slots.insert(name.into(), value);
    }

    pub fn one(&self, name: &str) -> Result<&BuilderInputObject, BuilderError> {
        match self.slots.get(name) {
            Some(BuilderInputValue::One(object)) => Ok(object),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    pub fn optional(&self, name: &str) -> Result<Option<&BuilderInputObject>, BuilderError> {
        match self.slots.get(name) {
            Some(BuilderInputValue::Optional(object)) => Ok(object.as_ref()),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "optional input slot '{name}' is missing"
            ))),
        }
    }

    pub fn many(&self, name: &str) -> Result<&[BuilderInputObject], BuilderError> {
        match self.slots.get(name) {
            Some(BuilderInputValue::Many(objects)) => Ok(objects),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "repeated input slot '{name}' is missing"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub enum BuilderInputValue {
    One(BuilderInputObject),
    Optional(Option<BuilderInputObject>),
    Many(Vec<BuilderInputObject>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildLogLevel {
    Info,
    Warn,
    Error,
}

impl fmt::Display for BuildLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warn => f.write_str("warn"),
            Self::Error => f.write_str("error"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildLogEvent {
    pub level: BuildLogLevel,
    pub phase: String,
    pub message: String,
    pub object_hash: Option<ObjectHash>,
    pub raw_log_path: Option<PathBuf>,
    pub details: Map<String, Value>,
}

pub trait BuildLogger: fmt::Debug + Send + Sync {
    fn log_event(&self, event: BuildLogEvent);

    fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, String>;
}

#[derive(Debug, Default)]
pub struct NoopBuildLogger;

impl BuildLogger for NoopBuildLogger {
    fn log_event(&self, _event: BuildLogEvent) {}

    fn allocate_raw_log_path(&self, _label: &str) -> Result<PathBuf, String> {
        Err("no build logger configured".to_string())
    }
}

#[derive(Clone)]
/// Builder execution context prepared by the runtime before a builder is invoked.
///
/// Contract:
/// - `state_dir` is a persistent builder-local state directory. It exists on entry and is not
///   cleaned by the runtime between builds.
/// - `temp_dir` is a per-build temporary directory. It exists and is empty on entry.
/// - Builders may create files and subdirectories inside both directories.
/// - The runtime owns cleanup of `temp_dir` after the builder finishes.
pub struct BuildContext {
    pub state_dir: PathBuf,
    pub temp_dir: PathBuf,
    logger: Arc<dyn BuildLogger>,
}

impl fmt::Debug for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildContext")
            .field("state_dir", &self.state_dir)
            .field("temp_dir", &self.temp_dir)
            .finish_non_exhaustive()
    }
}

impl BuildContext {
    pub fn with_noop_logger(state_dir: PathBuf, temp_dir: PathBuf) -> Self {
        Self {
            state_dir,
            temp_dir,
            logger: Arc::new(NoopBuildLogger),
        }
    }

    pub fn with_logger(mut self, logger: Arc<dyn BuildLogger>) -> Self {
        self.logger = logger;
        self
    }

    pub fn log_event(
        &self,
        level: BuildLogLevel,
        phase: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.log_event_with_details(level, phase, message, None, None, Map::new());
    }

    pub fn log_event_with_details(
        &self,
        level: BuildLogLevel,
        phase: impl Into<String>,
        message: impl Into<String>,
        object_hash: Option<ObjectHash>,
        raw_log_path: Option<PathBuf>,
        details: Map<String, Value>,
    ) {
        self.logger.log_event(BuildLogEvent {
            level,
            phase: phase.into(),
            message: message.into(),
            object_hash,
            raw_log_path,
            details,
        });
    }

    pub fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, BuilderError> {
        self.logger
            .allocate_raw_log_path(label)
            .map_err(BuilderError::ExecutionFailed)
    }

    pub fn write_raw_log(&self, label: &str, content: &str) -> Option<PathBuf> {
        let path = match self.allocate_raw_log_path(label) {
            Ok(path) => path,
            Err(_) => return None,
        };

        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            self.log_event(
                BuildLogLevel::Warn,
                "log-warning",
                format!(
                    "failed to create raw log directory '{}': {error}",
                    parent.display()
                ),
            );
            return None;
        }

        if let Err(error) = crate::fsutil::write_atomic(&path, content) {
            self.log_event(
                BuildLogLevel::Warn,
                "log-warning",
                format!("failed to write raw log '{}': {error}", path.display()),
            );
            return None;
        }

        Some(path)
    }
}

#[derive(Debug, Clone)]
pub struct StagedBuildResult {
    pub meta: Map<String, Value>,
    pub staged_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultInputIdentity {
    pub object_hash: ObjectHash,
    pub meta_hash: MetaHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub build_key: BuildKey,
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    pub meta_hash: MetaHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub meta: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRecord {
    pub result_key: BuildKey,
    pub object_hash: ObjectHash,
    pub meta_hash: MetaHash,
    pub created_at: Option<String>,
    pub inputs: Vec<ResultInputIdentity>,
    pub meta: Map<String, Value>,
}

mod serde_object_hash {
    use fsobj_hash::ObjectHash;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};
    use std::str::FromStr;

    pub fn serialize<S>(value: &ObjectHash, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ObjectHash, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        ObjectHash::from_str(&value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone)]
pub struct PublishedBuild {
    pub build: Build,
    pub result: ResultRecord,
    pub object_path: PathBuf,
}

pub trait Builder {
    fn spec(&self) -> &'static BuilderSpec;

    fn build_erased(
        &self,
        config: Value,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError>;
}

pub trait TypedBuilder {
    type Config: DeserializeOwned;

    fn spec(&self) -> &'static BuilderSpec;

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError>;
}

impl<T> Builder for T
where
    T: TypedBuilder,
{
    fn spec(&self) -> &'static BuilderSpec {
        <T as TypedBuilder>::spec(self)
    }

    fn build_erased(
        &self,
        config: Value,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        let config = serde_json::from_value(config).map_err(|error| {
            BuilderError::InvalidRecipe(format!("invalid builder config: {error}"))
        })?;
        self.build_typed(config, inputs, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_builder_object() -> BuilderInputObject {
        BuilderInputObject {
            object_path: PathBuf::from("/tmp/object"),
            meta: Map::new(),
        }
    }

    #[test]
    fn builder_inputs_helpers_work() {
        let object = sample_builder_object();
        let mut inputs = BuilderInputs::empty();
        inputs.insert("script", BuilderInputValue::One(object.clone()));
        inputs.insert("base", BuilderInputValue::Optional(None));
        inputs.insert(
            "sources",
            BuilderInputValue::Many(vec![object.clone(), object.clone()]),
        );

        assert_eq!(
            inputs.one("script").unwrap().object_path,
            object.object_path
        );
        assert!(inputs.optional("base").unwrap().is_none());
        assert_eq!(inputs.many("sources").unwrap().len(), 2);
        assert!(matches!(
            inputs.one("sources"),
            Err(BuilderError::ExecutionFailed(_))
        ));
    }

    #[derive(Debug)]
    struct DummyBuilder;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct DummyConfig {
        kind: String,
    }

    static DUMMY_SPEC: BuilderSpec = BuilderSpec {
        tag: "Dummy",
        inputs: &[],
    };

    impl TypedBuilder for DummyBuilder {
        type Config = DummyConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &DUMMY_SPEC
        }

        fn build_typed(
            &self,
            config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            assert_eq!(
                config,
                DummyConfig {
                    kind: "demo".into()
                }
            );
            assert!(inputs.is_empty());
            assert_eq!(cx.state_dir, PathBuf::from("/tmp/builder"));
            assert_eq!(cx.temp_dir, PathBuf::from("/tmp/tmp"));
            Ok(StagedBuildResult {
                meta: Map::from_iter([("kind".to_string(), Value::String(config.kind))]),
                staged_path: PathBuf::from("/tmp/out"),
            })
        }
    }

    #[test]
    fn typed_builder_adapter_decodes_config() {
        let builder = DummyBuilder;
        let mut cx = BuildContext::with_noop_logger(
            PathBuf::from("/tmp/builder"),
            PathBuf::from("/tmp/tmp"),
        );

        let result = builder
            .build_erased(
                serde_json::json!({ "kind": "demo" }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert_eq!(result.meta["kind"], Value::String("demo".to_string()));
    }

    #[test]
    fn typed_builder_adapter_exposes_typed_spec() {
        let builder = DummyBuilder;
        let erased: &dyn Builder = &builder;

        assert_eq!(erased.spec().tag, "Dummy");
        assert!(erased.spec().inputs.is_empty());
    }
}
