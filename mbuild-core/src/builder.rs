use crate::BuilderError;
use crate::cas::BuildKey;
use fsobj_hash::ObjectHash;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

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

#[derive(Debug, Clone)]
pub struct ResolvedObject {
    pub object_hash: ObjectHash,
    pub build_key: BuildKey,
    pub kind: String,
    pub attrs: Map<String, Value>,
    pub object_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedInputs {
    slots: BTreeMap<String, ResolvedInputValue>,
}

impl ResolvedInputs {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new(slots: BTreeMap<String, ResolvedInputValue>) -> Self {
        Self { slots }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn insert(&mut self, name: impl Into<String>, value: ResolvedInputValue) {
        self.slots.insert(name.into(), value);
    }

    pub fn one(&self, name: &str) -> Result<&ResolvedObject, BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedInputValue::One(object)) => Ok(object),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    pub fn optional(&self, name: &str) -> Result<Option<&ResolvedObject>, BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedInputValue::Optional(object)) => Ok(object.as_ref()),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "optional input slot '{name}' is missing"
            ))),
        }
    }

    pub fn many(&self, name: &str) -> Result<&[ResolvedObject], BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedInputValue::Many(objects)) => Ok(objects),
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
pub enum ResolvedInputValue {
    One(ResolvedObject),
    Optional(Option<ResolvedObject>),
    Many(Vec<ResolvedObject>),
}

#[derive(Debug, Clone)]
pub struct BuildContext {
    pub workspace_root: PathBuf,
    pub builder_root: PathBuf,
    pub temp_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProducerInfo {
    pub builder: String,
}

#[derive(Debug, Clone)]
pub struct StagedBuildResult {
    pub kind: String,
    pub producer: ProducerInfo,
    pub input_object_hashes: Vec<ObjectHash>,
    pub attrs: Map<String, Value>,
    pub staged_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BuildRecord {
    pub build_key: BuildKey,
    pub object_hash: ObjectHash,
    pub kind: String,
    pub producer: ProducerInfo,
    pub input_object_hashes: Vec<ObjectHash>,
    pub attrs: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct PublishedBuild {
    pub record: BuildRecord,
    pub object_path: PathBuf,
}

pub trait Builder {
    fn spec(&self) -> &'static BuilderSpec;

    fn build_erased(
        &self,
        config: Value,
        inputs: ResolvedInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError>;
}

pub trait TypedBuilder {
    type Config: DeserializeOwned;

    fn spec(&self) -> &'static BuilderSpec;

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: ResolvedInputs,
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
        inputs: ResolvedInputs,
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
    use std::str::FromStr;

    fn sample_object() -> ResolvedObject {
        ResolvedObject {
            object_hash: ObjectHash::from_str(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            build_key: BuildKey::from_str(
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap(),
            kind: "build-script".to_string(),
            attrs: Map::new(),
            object_path: PathBuf::from("/tmp/object"),
        }
    }

    #[test]
    fn resolved_inputs_helpers_work() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", ResolvedInputValue::One(object.clone()));
        inputs.insert("base", ResolvedInputValue::Optional(None));
        inputs.insert(
            "sources",
            ResolvedInputValue::Many(vec![object.clone(), object.clone()]),
        );

        assert_eq!(inputs.one("script").unwrap().kind, "build-script");
        assert!(inputs.optional("base").unwrap().is_none());
        assert_eq!(inputs.many("sources").unwrap().len(), 2);
        assert!(matches!(
            inputs.one("sources"),
            Err(BuilderError::ExecutionFailed(_))
        ));
    }

    #[test]
    fn resolved_inputs_missing_and_wrong_arity_are_errors() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", ResolvedInputValue::One(object));

        assert!(matches!(
            inputs.optional("script"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.many("script"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.one("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.optional("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.many("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
    }

    struct DummyBuilder;

    static DUMMY_SPEC: BuilderSpec = BuilderSpec {
        tag: "Dummy",
        inputs: &[],
    };

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DummyConfig {
        foo: String,
    }

    impl TypedBuilder for DummyBuilder {
        type Config = DummyConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &DUMMY_SPEC
        }

        fn build_typed(
            &self,
            config: Self::Config,
            _inputs: ResolvedInputs,
            _cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            Ok(StagedBuildResult {
                kind: config.foo,
                producer: ProducerInfo {
                    builder: "dummy".to_string(),
                },
                input_object_hashes: vec![],
                attrs: Map::new(),
                staged_path: PathBuf::from("/tmp/staged"),
            })
        }
    }

    #[test]
    fn typed_builder_adapter_decodes_config() {
        let builder = DummyBuilder;
        let mut cx = BuildContext {
            workspace_root: PathBuf::from("/tmp/ws"),
            builder_root: PathBuf::from("/tmp/ws/dummy"),
            temp_root: PathBuf::from("/tmp/ws/dummy/tmp"),
        };

        let result = builder
            .build_erased(
                serde_json::json!({ "foo": "ok" }),
                ResolvedInputs::empty(),
                &mut cx,
            )
            .unwrap();
        assert_eq!(result.kind, "ok");

        let error = builder
            .build_erased(
                serde_json::json!({ "foo": "ok", "extra": true }),
                ResolvedInputs::empty(),
                &mut cx,
            )
            .unwrap_err();
        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    #[test]
    fn typed_builder_adapter_exposes_typed_spec() {
        let builder = DummyBuilder;
        assert_eq!(Builder::spec(&builder).tag, "Dummy");
        assert!(Builder::spec(&builder).inputs.is_empty());
    }
}
