use bobr_runtime::runtime_provider::RuntimeProvider;
use bobr_store::fs_tree::FsTree;
use fsobj_hash::ObjectHash;
use mbuild_core::{
    BuildLogEvent, BuildLogLevel, BuildLogger, BuilderError, CancellationToken, NoopBuildLogger,
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct InputSpec {
    pub required_inputs: &'static [&'static str],
    pub optional_inputs: &'static [&'static str],
    pub allow_extra_inputs: bool,
}

impl InputSpec {
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_builder("input spec")
    }

    pub fn validate_for_builder(&self, builder_tag: &str) -> Result<(), String> {
        let mut required = BTreeSet::new();
        for name in self.required_inputs {
            validate_input_name(name).map_err(|error| {
                format!(
                    "builder '{}' declares invalid required input '{}': {error}",
                    builder_tag, name
                )
            })?;
            if !required.insert(*name) {
                return Err(format!(
                    "builder '{}' declares duplicate required input '{}'",
                    builder_tag, name
                ));
            }
        }

        let mut optional = BTreeSet::new();
        for name in self.optional_inputs {
            validate_input_name(name).map_err(|error| {
                format!(
                    "builder '{}' declares invalid optional input '{}': {error}",
                    builder_tag, name
                )
            })?;
            if !optional.insert(*name) {
                return Err(format!(
                    "builder '{}' declares duplicate optional input '{}'",
                    builder_tag, name
                ));
            }
            if required.contains(name) {
                return Err(format!(
                    "builder '{}' declares input '{}' as both required and optional",
                    builder_tag, name
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

pub(crate) fn validate_input_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("input name must not be empty".to_string());
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "input name '{name}' must start with an ASCII letter or underscore"
        ));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(format!(
            "input name '{name}' must contain only ASCII letters, digits, and underscores"
        ));
    }
    Ok(())
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

    pub fn extra<'a>(&'a self, spec: &InputSpec, name: &str) -> Option<&'a BuilderInputObject> {
        if spec.is_reserved_input(name) {
            None
        } else {
            self.slots.get(name)
        }
    }

    pub fn extras<'a>(
        &'a self,
        spec: &'a InputSpec,
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
    runtime: RuntimeProvider,
    fs_tree: Option<FsTree>,
}

impl fmt::Debug for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildContext")
            .field("temp_dir", &self.temp_dir)
            .field("runtime_backend", &self.runtime.backend())
            .field("has_fs_tree", &self.fs_tree.is_some())
            .finish_non_exhaustive()
    }
}

impl BuildContext {
    pub fn with_noop_logger(temp_dir: PathBuf) -> Self {
        Self {
            temp_dir,
            logger: Arc::new(NoopBuildLogger),
            cancellation: CancellationToken::new(),
            runtime: RuntimeProvider::host(),
            fs_tree: None,
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

    pub fn with_runtime_provider(mut self, runtime: RuntimeProvider) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn with_fs_tree(mut self, fs_tree: FsTree) -> Self {
        self.fs_tree = Some(fs_tree);
        self
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn runtime(&self) -> &RuntimeProvider {
        &self.runtime
    }

    pub fn fs_tree(&self) -> Result<FsTree, BuilderError> {
        self.fs_tree.clone().ok_or_else(|| {
            BuilderError::ExecutionFailed(
                "builder requires store fs-tree operations, but none were provided".to_string(),
            )
        })
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

pub trait Builder: Send + Sync {
    fn tag(&self) -> &'static str;

    fn spec(&self) -> &'static InputSpec;

    fn build_erased(
        &self,
        config: Value,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError>;
}

pub trait TypedBuilder: Send + Sync {
    type Config: DeserializeOwned;

    fn tag(&self) -> &'static str;

    fn spec(&self) -> &'static InputSpec;

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
    fn tag(&self) -> &'static str {
        <T as TypedBuilder>::tag(self)
    }

    fn spec(&self) -> &'static InputSpec {
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
    use bobr_runtime::runtime_provider::{RuntimeBackend, RuntimeProvider};
    use serde::Deserialize;

    fn sample_builder_object() -> BuilderInputObject {
        BuilderInputObject {
            path: PathBuf::from("/tmp/object"),
        }
    }

    #[test]
    fn input_spec_validate_accepts_distinct_inputs() {
        let spec = InputSpec {
            required_inputs: &["rootfs", "toolchain"],
            optional_inputs: &["source"],
            allow_extra_inputs: true,
        };

        spec.validate_for_builder("Test").unwrap();
    }

    #[test]
    fn input_spec_validate_rejects_duplicate_required_inputs() {
        let spec = InputSpec {
            required_inputs: &["rootfs", "rootfs"],
            optional_inputs: &[],
            allow_extra_inputs: true,
        };

        assert_eq!(
            spec.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares duplicate required input 'rootfs'"
        );
    }

    #[test]
    fn input_spec_validate_rejects_duplicate_optional_inputs() {
        let spec = InputSpec {
            required_inputs: &[],
            optional_inputs: &["source", "source"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            spec.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares duplicate optional input 'source'"
        );
    }

    #[test]
    fn input_spec_validate_rejects_required_optional_overlap() {
        let spec = InputSpec {
            required_inputs: &["rootfs"],
            optional_inputs: &["rootfs"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            spec.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares input 'rootfs' as both required and optional"
        );
    }

    #[test]
    fn input_spec_validate_rejects_invalid_declared_input_names() {
        let required = InputSpec {
            required_inputs: &["bad-name"],
            optional_inputs: &[],
            allow_extra_inputs: true,
        };
        let optional = InputSpec {
            required_inputs: &[],
            optional_inputs: &["1bad"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            required.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares invalid required input 'bad-name': input name 'bad-name' must contain only ASCII letters, digits, and underscores"
        );
        assert_eq!(
            optional.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares invalid optional input '1bad': input name '1bad' must start with an ASCII letter or underscore"
        );
    }

    #[test]
    fn builder_inputs_helpers_work() {
        let object = sample_builder_object();
        let mut inputs = BuilderInputs::empty();
        inputs.insert("script", object.clone());
        inputs.insert("source", object.clone());
        inputs.insert("patch", object.clone());

        let spec = InputSpec {
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

    static DUMMY_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    impl TypedBuilder for DummyBuilder {
        type Config = DummyConfig;

        fn tag(&self) -> &'static str {
            "Dummy"
        }

        fn spec(&self) -> &'static InputSpec {
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
    fn build_context_defaults_to_host_runtime() {
        let cx = BuildContext::with_noop_logger(PathBuf::from("/tmp/tmp"));

        assert_eq!(cx.runtime().backend(), RuntimeBackend::Host);
    }

    #[test]
    fn build_context_can_override_runtime_provider() {
        let cx = BuildContext::with_noop_logger(PathBuf::from("/tmp/tmp"))
            .with_runtime_provider(RuntimeProvider::namespace());

        assert_eq!(cx.runtime().backend(), RuntimeBackend::Namespace);
    }

    #[test]
    fn build_context_reports_missing_fs_tree() {
        let cx = BuildContext::with_noop_logger(PathBuf::from("/tmp/tmp"));

        let error = cx.fs_tree().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires store fs-tree operations")
        );
    }

    #[test]
    fn typed_builder_adapter_exposes_typed_spec() {
        let builder = DummyBuilder;
        let erased: &dyn Builder = &builder;

        assert_eq!(erased.tag(), "Dummy");
        assert!(erased.spec().required_inputs.is_empty());
        assert!(erased.spec().optional_inputs.is_empty());
        assert!(!erased.spec().allow_extra_inputs);
    }
}
