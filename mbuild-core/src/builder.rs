use crate::BuilderError;
use crate::cancellation::CancellationToken;
use crate::logging::{BuildLogEvent, BuildLogLevel, BuildLogger, NoopBuildLogger};
use crate::origin::ParsedOrigin;
use fsobj_hash::ObjectHash;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Workspace paths assigned to one concrete builder run.
///
/// A workspace belongs to a single per-run builder object. It contains the
/// subject log directory, the raw-log subdirectory, and a per-run temporary
/// directory. Allocation is handled outside `mbuild-core`; this type is the
/// builder-facing value object passed to per-run builders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    log_dir: PathBuf,
    raw_log_dir: PathBuf,
    temp_dir: PathBuf,
}

impl Workspace {
    /// Creates a workspace from already allocated paths.
    pub fn new(log_dir: PathBuf, raw_log_dir: PathBuf, temp_dir: PathBuf) -> Self {
        Self {
            log_dir,
            raw_log_dir,
            temp_dir,
        }
    }

    /// Returns the per-run log directory.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Returns the per-run raw log directory.
    pub fn raw_log_dir(&self) -> &Path {
        &self.raw_log_dir
    }

    /// Returns the per-run temporary directory.
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }
}

/// Common base trait for concrete per-run builder objects.
///
/// This trait intentionally exposes only the builder tag. Workspace and serial
/// allocation are store/runtime concerns and are not available through this
/// trait.
pub trait BuilderObjectBase: Send + Sync {
    /// Returns the builder tag used for logging and recipe identity.
    fn tag(&self) -> &str;
}

/// Common base trait for builder classes that create per-run objects.
pub trait BuilderClassBase: Send + Sync {
    /// Data needed to create one concrete per-run object.
    type Init;
    /// Concrete per-run object created by this builder class.
    type Object: BuilderObjectBase;

    /// Returns the builder tag advertised by this class.
    fn tag(&self) -> &'static str;

    /// Creates one concrete per-run object from runtime-allocated state.
    fn create_object(&self, init: Self::Init) -> Self::Object;
}

/// Runtime-allocated state used to create a normal per-run builder object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderRunInit {
    pub recipe_name: Option<String>,
    pub build_key: String,
    pub workspace: Workspace,
}

/// Runtime-allocated state used to create a per-run source builder object.
#[derive(Debug, Clone)]
pub struct SourceBuilderInit {
    pub recipe_name: String,
    pub build_key: String,
    pub declared_object_hash: ObjectHash,
    pub origin: Option<Box<dyn ParsedOrigin>>,
    pub workspace: Workspace,
}

/// Concrete per-run object for a normal builder node.
#[derive(Debug, Clone)]
pub struct BuilderRun {
    tag: String,
    recipe_name: Option<String>,
    build_key: String,
    workspace: Workspace,
}

impl BuilderRun {
    /// Creates a per-run builder object from its identity and workspace.
    pub fn new(
        tag: impl Into<String>,
        recipe_name: Option<String>,
        build_key: impl Into<String>,
        workspace: Workspace,
    ) -> Self {
        Self {
            tag: tag.into(),
            recipe_name,
            build_key: build_key.into(),
            workspace,
        }
    }

    /// Returns the builder tag for this run.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Returns the recipe name associated with this run, when there is one.
    pub fn recipe_name(&self) -> Option<&str> {
        self.recipe_name.as_deref()
    }

    /// Returns the full build key string associated with this run.
    pub fn build_key(&self) -> &str {
        &self.build_key
    }

    /// Returns the per-run log directory.
    pub fn log_dir(&self) -> &Path {
        self.workspace.log_dir()
    }

    /// Returns the per-run raw log directory.
    pub fn raw_log_dir(&self) -> &Path {
        self.workspace.raw_log_dir()
    }

    /// Returns the per-run temporary directory.
    pub fn temp_dir(&self) -> &Path {
        self.workspace.temp_dir()
    }
}

impl BuilderObjectBase for BuilderRun {
    fn tag(&self) -> &str {
        &self.tag
    }
}

/// Concrete per-run object for a source node.
#[derive(Debug, Clone)]
pub struct SourceBuilder {
    run: BuilderRun,
    declared_object_hash: ObjectHash,
    origin: Option<Box<dyn ParsedOrigin>>,
}

impl SourceBuilder {
    /// Creates a per-run source builder object.
    pub fn new(
        recipe_name: String,
        build_key: impl Into<String>,
        declared_object_hash: ObjectHash,
        origin: Option<Box<dyn ParsedOrigin>>,
        workspace: Workspace,
    ) -> Self {
        Self {
            run: BuilderRun::new("Source", Some(recipe_name), build_key, workspace),
            declared_object_hash,
            origin,
        }
    }

    /// Returns the recipe name associated with this source.
    pub fn recipe_name(&self) -> &str {
        self.run
            .recipe_name()
            .expect("source builders are always created with a recipe name")
    }

    /// Returns the source builder tag.
    pub fn tag(&self) -> &str {
        self.run.tag()
    }

    /// Returns the full build key string associated with this source.
    pub fn build_key(&self) -> &str {
        self.run.build_key()
    }

    /// Returns the declared object hash for this source.
    pub fn declared_object_hash(&self) -> ObjectHash {
        self.declared_object_hash
    }

    /// Returns the parsed origin, when one was declared.
    pub fn origin(&self) -> Option<&dyn ParsedOrigin> {
        self.origin.as_deref()
    }

    /// Returns the per-run log directory.
    pub fn log_dir(&self) -> &Path {
        self.run.log_dir()
    }

    /// Returns the per-run raw log directory.
    pub fn raw_log_dir(&self) -> &Path {
        self.run.raw_log_dir()
    }

    /// Returns the per-run temporary directory.
    pub fn temp_dir(&self) -> &Path {
        self.run.temp_dir()
    }
}

impl BuilderObjectBase for SourceBuilder {
    fn tag(&self) -> &str {
        self.run.tag()
    }
}

/// Builder class for source nodes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SourceBuilderClass;

impl BuilderClassBase for SourceBuilderClass {
    type Init = SourceBuilderInit;
    type Object = SourceBuilder;

    fn tag(&self) -> &'static str {
        "Source"
    }

    fn create_object(&self, init: Self::Init) -> Self::Object {
        SourceBuilder::new(
            init.recipe_name,
            init.build_key,
            init.declared_object_hash,
            init.origin,
            init.workspace,
        )
    }
}

#[derive(Debug)]
pub struct BuilderSpec {
    pub tag: &'static str,
    pub required_inputs: &'static [&'static str],
    pub optional_inputs: &'static [&'static str],
    pub allow_extra_inputs: bool,
}

impl BuilderSpec {
    pub fn validate(&self) -> Result<(), String> {
        let mut required = BTreeSet::new();
        for name in self.required_inputs {
            if !required.insert(*name) {
                return Err(format!(
                    "builder '{}' declares duplicate required input '{}'",
                    self.tag, name
                ));
            }
        }

        let mut optional = BTreeSet::new();
        for name in self.optional_inputs {
            if !optional.insert(*name) {
                return Err(format!(
                    "builder '{}' declares duplicate optional input '{}'",
                    self.tag, name
                ));
            }
            if required.contains(name) {
                return Err(format!(
                    "builder '{}' declares input '{}' as both required and optional",
                    self.tag, name
                ));
            }
        }

        Ok(())
    }

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

/// Builder execution context prepared by the runtime before a builder is invoked.
///
/// Contract:
/// - `temp_dir` is a per-run temporary directory. It exists and is empty on entry.
/// - Builders may create files and subdirectories inside `temp_dir`.
/// - The runtime owns cleanup of `temp_dir` after the builder finishes.
#[derive(Clone)]
pub struct BuildContext {
    /// Per-run temporary directory owned by the runtime.
    pub temp_dir: PathBuf,
    logger: Arc<dyn BuildLogger>,
    cancellation: CancellationToken,
}

impl fmt::Debug for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildContext")
            .field("temp_dir", &self.temp_dir)
            .finish_non_exhaustive()
    }
}

impl BuildContext {
    pub fn with_noop_logger(temp_dir: PathBuf) -> Self {
        Self {
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

        if let Err(error) = write_raw_log_atomic(&path, content) {
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

fn write_raw_log_atomic(path: &Path, content: &str) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid file name for raw log path '{}'", path.display()))?;
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system time before UNIX_EPOCH: {error}"))?
        .as_nanos();
    let tmp_path = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        now_nanos
    ));

    fs::write(&tmp_path, content).map_err(|error| {
        format!(
            "failed to write temporary raw log '{}': {error}",
            tmp_path.display()
        )
    })?;
    fs::rename(&tmp_path, path).map_err(|error| {
        format!(
            "failed to move temporary raw log '{}' to '{}': {error}",
            tmp_path.display(),
            path.display()
        )
    })
}

#[derive(Debug, Clone)]
pub struct StagedBuildResult {
    pub staged_path: PathBuf,
    pub object_hash: Option<ObjectHash>,
}

pub trait Builder: BuilderClassBase<Init = BuilderRunInit, Object = BuilderRun> {
    fn spec(&self) -> &'static BuilderSpec;

    fn build_erased(
        &self,
        config: Value,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError>;
}

pub trait TypedBuilder: Send + Sync {
    type Config: DeserializeOwned;

    fn spec(&self) -> &'static BuilderSpec;

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError>;
}

impl<T> BuilderClassBase for T
where
    T: TypedBuilder,
{
    type Init = BuilderRunInit;
    type Object = BuilderRun;

    fn tag(&self) -> &'static str {
        <T as TypedBuilder>::spec(self).tag
    }

    fn create_object(&self, init: Self::Init) -> Self::Object {
        BuilderRun::new(self.tag(), init.recipe_name, init.build_key, init.workspace)
    }
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
    use serde::Deserialize;

    fn sample_builder_object() -> BuilderInputObject {
        BuilderInputObject {
            path: PathBuf::from("/tmp/object"),
        }
    }

    #[test]
    fn builder_spec_validate_accepts_distinct_inputs() {
        let spec = BuilderSpec {
            tag: "Test",
            required_inputs: &["rootfs", "toolchain"],
            optional_inputs: &["source"],
            allow_extra_inputs: true,
        };

        spec.validate().unwrap();
    }

    #[test]
    fn builder_spec_validate_rejects_duplicate_required_inputs() {
        let spec = BuilderSpec {
            tag: "Test",
            required_inputs: &["rootfs", "rootfs"],
            optional_inputs: &[],
            allow_extra_inputs: true,
        };

        assert_eq!(
            spec.validate().unwrap_err(),
            "builder 'Test' declares duplicate required input 'rootfs'"
        );
    }

    #[test]
    fn builder_spec_validate_rejects_duplicate_optional_inputs() {
        let spec = BuilderSpec {
            tag: "Test",
            required_inputs: &[],
            optional_inputs: &["source", "source"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            spec.validate().unwrap_err(),
            "builder 'Test' declares duplicate optional input 'source'"
        );
    }

    #[test]
    fn builder_spec_validate_rejects_required_optional_overlap() {
        let spec = BuilderSpec {
            tag: "Test",
            required_inputs: &["rootfs"],
            optional_inputs: &["rootfs"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            spec.validate().unwrap_err(),
            "builder 'Test' declares input 'rootfs' as both required and optional"
        );
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
        let mut cx = BuildContext::with_noop_logger(PathBuf::from("/tmp/tmp"));

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

    #[test]
    fn typed_builder_class_creates_builder_run_object() {
        let builder = DummyBuilder;
        let erased: &dyn Builder = &builder;
        let workspace = Workspace::new(
            PathBuf::from("/tmp/dummy/log"),
            PathBuf::from("/tmp/dummy/log/raw"),
            PathBuf::from("/tmp/dummy/tmp"),
        );

        let run = erased.create_object(BuilderRunInit {
            recipe_name: Some("demo".to_string()),
            build_key: "build-key".to_string(),
            workspace: workspace.clone(),
        });

        assert_eq!(run.tag(), "Dummy");
        assert_eq!(run.recipe_name(), Some("demo"));
        assert_eq!(run.build_key(), "build-key");
        assert_eq!(run.log_dir(), workspace.log_dir());
        assert_eq!(run.raw_log_dir(), workspace.raw_log_dir());
        assert_eq!(run.temp_dir(), workspace.temp_dir());
    }

    #[test]
    fn source_builder_class_creates_source_object() {
        let object_hash = "0000000000000000000000000000000000000000000000000000000000000000"
            .parse::<ObjectHash>()
            .unwrap();
        let workspace = Workspace::new(
            PathBuf::from("/tmp/source/log"),
            PathBuf::from("/tmp/source/log/raw"),
            PathBuf::from("/tmp/source/tmp"),
        );

        let source = SourceBuilderClass.create_object(SourceBuilderInit {
            recipe_name: "source-demo".to_string(),
            build_key: "source-key".to_string(),
            declared_object_hash: object_hash,
            origin: None,
            workspace: workspace.clone(),
        });

        assert_eq!(source.tag(), "Source");
        assert_eq!(source.recipe_name(), "source-demo");
        assert_eq!(source.build_key(), "source-key");
        assert_eq!(source.declared_object_hash(), object_hash);
        assert!(source.origin().is_none());
        assert_eq!(source.log_dir(), workspace.log_dir());
        assert_eq!(source.raw_log_dir(), workspace.raw_log_dir());
        assert_eq!(source.temp_dir(), workspace.temp_dir());
    }
}
