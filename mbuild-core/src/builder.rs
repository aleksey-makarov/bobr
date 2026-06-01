use crate::BuilderError;
use crate::cancellation::CancellationToken;
use crate::cas::{BuildKey, ResultId, ReuseKey};
use crate::logging::{BuildLogEvent, BuildLogLevel, BuildLogger, NoopBuildLogger};
use fsobj_hash::ObjectHash;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug)]
pub struct BuilderSpec {
    pub tag: &'static str,
    pub required_inputs: &'static [&'static str],
    pub optional_inputs: &'static [&'static str],
    pub allow_extra_inputs: bool,
}

impl BuilderSpec {
    pub fn reserved_input_names(&self) -> impl Iterator<Item = &'static str> {
        self.required_inputs
            .iter()
            .copied()
            .chain(self.optional_inputs.iter().copied())
    }

    pub fn is_required_input(&self, name: &str) -> bool {
        self.required_inputs.contains(&name)
    }

    pub fn is_optional_input(&self, name: &str) -> bool {
        self.optional_inputs.contains(&name)
    }

    pub fn is_reserved_input(&self, name: &str) -> bool {
        self.is_required_input(name) || self.is_optional_input(name)
    }

    pub fn ordered_present_input_names<'a, T>(
        &self,
        inputs: &'a BTreeMap<String, T>,
    ) -> Vec<&'a str> {
        let mut ordered = Vec::new();
        for name in self.required_inputs {
            if inputs.contains_key(*name) {
                ordered.push(*name);
            }
        }
        for name in self.optional_inputs {
            if inputs.contains_key(*name) {
                ordered.push(*name);
            }
        }
        for name in inputs.keys() {
            if !self.is_reserved_input(name) {
                ordered.push(name.as_str());
            }
        }
        ordered
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderInputObject {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct BuilderInputs {
    slots: BTreeMap<String, BuilderInputObject>,
}

impl BuilderInputs {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new(slots: BTreeMap<String, BuilderInputObject>) -> Self {
        Self { slots }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn insert(&mut self, name: impl Into<String>, value: BuilderInputObject) {
        self.slots.insert(name.into(), value);
    }

    pub fn required(&self, name: &str) -> Result<&BuilderInputObject, BuilderError> {
        match self.slots.get(name) {
            Some(object) => Ok(object),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    pub fn optional(&self, name: &str) -> Option<&BuilderInputObject> {
        self.slots.get(name)
    }

    pub fn get(&self, name: &str) -> Option<&BuilderInputObject> {
        self.slots.get(name)
    }

    pub fn extra<'a>(&'a self, spec: &BuilderSpec, name: &str) -> Option<&'a BuilderInputObject> {
        if spec.is_reserved_input(name) {
            None
        } else {
            self.slots.get(name)
        }
    }

    pub fn extras<'a>(
        &'a self,
        spec: &'a BuilderSpec,
    ) -> impl Iterator<Item = (&'a str, &'a BuilderInputObject)> + 'a {
        self.slots.iter().filter_map(move |(name, object)| {
            if spec.is_reserved_input(name) {
                None
            } else {
                Some((name.as_str(), object))
            }
        })
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
    cancellation: CancellationToken,
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
            cancellation: CancellationToken::new(),
        }
    }

    pub fn with_logger(mut self, logger: Arc<dyn BuildLogger>) -> Self {
        self.logger = logger;
        self
    }

    pub fn with_cancellation_token(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn check_cancelled(&self) -> Result<(), BuilderError> {
        self.cancellation.check_cancelled()
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
    pub staged_path: PathBuf,
    pub object_hash: Option<ObjectHash>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultInputIdentity {
    pub object_hash: ObjectHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub build_key: BuildKey,
    pub result_id: ResultId,
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRecord {
    pub result_id: ResultId,
    pub object_hash: ObjectHash,
    pub created_at: Option<String>,
    pub inputs: Vec<ResultInputIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizedResult {
    pub result_id: ResultId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_key: Option<BuildKey>,
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
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
    pub reuse_key: ReuseKey,
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
            path: PathBuf::from("/tmp/object"),
        }
    }

    #[test]
    fn builder_inputs_helpers_work() {
        let object = sample_builder_object();
        let mut inputs = BuilderInputs::empty();
        inputs.insert("script", object.clone());
        inputs.insert("source", object.clone());
        inputs.insert("patch", object.clone());

        let spec = BuilderSpec {
            tag: "Sandbox",
            required_inputs: &["image"],
            optional_inputs: &["base"],
            allow_extra_inputs: true,
        };

        assert_eq!(inputs.required("script").unwrap().path, object.path);
        assert!(inputs.optional("base").is_none());
        assert!(inputs.extra(&spec, "source").is_some());
        assert_eq!(inputs.extras(&spec).count(), 3);
    }

    #[derive(Debug)]
    struct DummyBuilder;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct DummyConfig {
        demo: String,
    }

    static DUMMY_SPEC: BuilderSpec = BuilderSpec {
        tag: "Dummy",
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
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
                    demo: "demo".into()
                }
            );
            assert!(inputs.is_empty());
            assert_eq!(cx.state_dir, PathBuf::from("/tmp/builder"));
            assert_eq!(cx.temp_dir, PathBuf::from("/tmp/tmp"));
            Ok(StagedBuildResult {
                staged_path: PathBuf::from("/tmp/out"),
                object_hash: None,
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
                serde_json::json!({ "demo": "demo" }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert_eq!(result.staged_path, PathBuf::from("/tmp/out"));
    }

    #[test]
    fn typed_builder_adapter_exposes_typed_spec() {
        let builder = DummyBuilder;
        let erased: &dyn Builder = &builder;

        assert_eq!(erased.spec().tag, "Dummy");
        assert!(erased.spec().required_inputs.is_empty());
        assert!(erased.spec().optional_inputs.is_empty());
        assert!(!erased.spec().allow_extra_inputs);
    }
}
