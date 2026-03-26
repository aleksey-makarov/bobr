use crate::BuilderError;
use crate::cas::BuildKey;
use fsobj_hash::ObjectHash;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
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
    pub allowed_kinds: &'static [&'static str],
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
    pub builder: String,
    pub name: String,
    pub build_key: BuildKey,
    pub message: String,
    pub object_hash: Option<ObjectHash>,
    pub raw_log_path: Option<PathBuf>,
    pub details: Map<String, Value>,
}

pub trait BuildLogger: fmt::Debug + Send + Sync {
    fn log_event(&self, event: BuildLogEvent);

    fn allocate_raw_log_path(
        &self,
        builder: &str,
        name: &str,
        build_key: BuildKey,
        label: &str,
    ) -> Result<PathBuf, String>;
}

#[derive(Debug, Default)]
pub struct NoopBuildLogger;

impl BuildLogger for NoopBuildLogger {
    fn log_event(&self, _event: BuildLogEvent) {}

    fn allocate_raw_log_path(
        &self,
        _builder: &str,
        _name: &str,
        _build_key: BuildKey,
        _label: &str,
    ) -> Result<PathBuf, String> {
        Err("no build logger configured".to_string())
    }
}

#[derive(Clone)]
pub struct BuildContext {
    pub workspace_root: PathBuf,
    pub builder_root: PathBuf,
    pub temp_root: PathBuf,
    pub build_key: BuildKey,
    pub builder_tag: String,
    pub build_name: String,
    logger: Arc<dyn BuildLogger>,
}

impl fmt::Debug for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildContext")
            .field("workspace_root", &self.workspace_root)
            .field("builder_root", &self.builder_root)
            .field("temp_root", &self.temp_root)
            .field("build_key", &self.build_key)
            .field("builder_tag", &self.builder_tag)
            .field("build_name", &self.build_name)
            .finish_non_exhaustive()
    }
}

impl BuildContext {
    pub fn with_noop_logger(
        workspace_root: PathBuf,
        builder_root: PathBuf,
        temp_root: PathBuf,
        build_key: BuildKey,
        builder_tag: impl Into<String>,
        build_name: impl Into<String>,
    ) -> Self {
        Self {
            workspace_root,
            builder_root,
            temp_root,
            build_key,
            builder_tag: builder_tag.into(),
            build_name: build_name.into(),
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
            builder: self.builder_tag.clone(),
            name: self.build_name.clone(),
            build_key: self.build_key,
            message: message.into(),
            object_hash,
            raw_log_path,
            details,
        });
    }

    pub fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, BuilderError> {
        self.logger
            .allocate_raw_log_path(&self.builder_tag, &self.build_name, self.build_key, label)
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

    pub fn logger(&self) -> &dyn BuildLogger {
        self.logger.as_ref()
    }

    pub fn builder_tag(&self) -> &str {
        &self.builder_tag
    }

    pub fn build_name(&self) -> &str {
        &self.build_name
    }

    pub fn build_key(&self) -> BuildKey {
        self.build_key
    }

    pub fn builder_root(&self) -> &Path {
        &self.builder_root
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerInfo {
    pub builder: String,
}

#[derive(Debug, Clone)]
pub struct StagedBuildResult {
    pub kind: String,
    pub producer: ProducerInfo,
    pub attrs: Map<String, Value>,
    pub staged_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ContainerImageDescriptor {
    pub image_ref: String,
    pub image_digest: String,
}

pub fn load_container_image_descriptor(path: &Path) -> Result<ContainerImageDescriptor, String> {
    let bytes = fs::read(path).map_err(|error| {
        format!(
            "failed to read container-image descriptor '{}': {error}",
            path.display()
        )
    })?;
    let descriptor: ContainerImageDescriptor =
        serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "failed to parse container-image descriptor '{}': {error}",
                path.display()
            )
        })?;
    if descriptor.image_ref.trim().is_empty() {
        return Err(format!(
            "container-image descriptor '{}' has empty 'image_ref'",
            path.display()
        ));
    }
    if descriptor.image_digest.trim().is_empty() {
        return Err(format!(
            "container-image descriptor '{}' has empty 'image_digest'",
            path.display()
        ));
    }
    Ok(descriptor)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub build_key: BuildKey,
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub kind: String,
    pub producer: ProducerInfo,
    pub attrs: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRecord {
    pub result_key: BuildKey,
    pub object_hash: ObjectHash,
    pub created_at: Option<String>,
    pub kind: String,
    pub producer: ProducerInfo,
    pub input_object_hashes: Vec<ObjectHash>,
    pub attrs: Map<String, Value>,
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

        assert_eq!(inputs.one("script").unwrap().object_path, object.object_path);
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
            assert_eq!(cx.workspace_root, PathBuf::from("/tmp/ws"));
            Ok(StagedBuildResult {
                kind: config.kind,
                producer: ProducerInfo {
                    builder: "dummy".to_string(),
                },
                attrs: Map::new(),
                staged_path: PathBuf::from("/tmp/out"),
            })
        }
    }

    #[test]
    fn typed_builder_adapter_decodes_config() {
        let builder = DummyBuilder;
        let mut cx = BuildContext::with_noop_logger(
            PathBuf::from("/tmp/ws"),
            PathBuf::from("/tmp/builder"),
            PathBuf::from("/tmp/tmp"),
            "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
            "Dummy",
            "dummy",
        );

        let result = builder
            .build_erased(
                serde_json::json!({ "kind": "demo" }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert_eq!(result.kind, "demo");
        assert_eq!(result.producer.builder, "dummy");
    }

    #[test]
    fn typed_builder_adapter_exposes_typed_spec() {
        let builder = DummyBuilder;
        let erased: &dyn Builder = &builder;

        assert_eq!(erased.spec().tag, "Dummy");
        assert!(erased.spec().inputs.is_empty());
    }
}
