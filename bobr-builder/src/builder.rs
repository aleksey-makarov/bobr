use crate::BuilderError;
use bobr_core::{
    BuildLogEvent, BuildLogLevel, BuildLogger, BuildStatus, CancellationToken, NoopBuildLogger,
};
use bobr_runtime::runtime_provider::RuntimeProvider;
use bobr_store::fs_tree::FsTree;
use fsobj_hash::ObjectHash;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// A builder's input contract: which named slots it requires, which are
/// optional, and whether extra (unlisted) inputs are accepted. Slots are
/// identified by name.
///
/// Whether an input is materialized into an fs-tree root or passed as the
/// object itself is decided by the input **name**, not the spec: a name
/// beginning with `_` is materialized (see `resolved_inputs`). The slot list
/// only declares which names are required/optional and whether extras are
/// allowed.
#[derive(Debug)]
pub struct InputSpec {
    /// Names of slots that must be present.
    pub required_inputs: &'static [&'static str],
    /// Names of slots that may be present.
    pub optional_inputs: &'static [&'static str],
    /// Whether inputs beyond the declared slots are allowed.
    pub allow_extra_inputs: bool,
}

impl InputSpec {
    /// Validates the spec itself: input names are well-formed, with no
    /// duplicates or required/optional conflicts.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_builder("input spec")
    }

    /// Like [`validate`](InputSpec::validate), with `builder_tag` included in
    /// error messages.
    pub fn validate_for_builder(&self, builder_tag: &str) -> Result<(), String> {
        let mut required = BTreeSet::new();
        for &name in self.required_inputs {
            validate_input_name(name).map_err(|error| {
                format!(
                    "builder '{}' declares invalid required input '{}': {error}",
                    builder_tag, name
                )
            })?;
            if !required.insert(name) {
                return Err(format!(
                    "builder '{}' declares duplicate required input '{}'",
                    builder_tag, name
                ));
            }
        }

        let mut optional = BTreeSet::new();
        for &name in self.optional_inputs {
            validate_input_name(name).map_err(|error| {
                format!(
                    "builder '{}' declares invalid optional input '{}': {error}",
                    builder_tag, name
                )
            })?;
            if !optional.insert(name) {
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

    /// Names of all declared (required + optional) slots.
    pub fn reserved_input_names(&self) -> impl Iterator<Item = &'static str> {
        self.required_inputs
            .iter()
            .copied()
            .chain(self.optional_inputs.iter().copied())
    }

    /// Whether `name` is a required slot.
    pub fn is_required_input(&self, name: &str) -> bool {
        self.required_inputs.contains(&name)
    }

    /// Whether `name` is an optional slot.
    pub fn is_optional_input(&self, name: &str) -> bool {
        self.optional_inputs.contains(&name)
    }

    /// Whether `name` is a declared (required or optional) slot.
    pub fn is_reserved_input(&self, name: &str) -> bool {
        self.is_required_input(name) || self.is_optional_input(name)
    }

    /// Present input names in canonical order: required, then optional, then
    /// any extras.
    pub fn ordered_present_input_names<'a, T>(
        &self,
        inputs: &'a BTreeMap<String, T>,
    ) -> Vec<&'a str> {
        let mut ordered = Vec::new();
        for &name in self.required_inputs {
            if inputs.contains_key(name) {
                ordered.push(name);
            }
        }
        for &name in self.optional_inputs {
            if inputs.contains_key(name) {
                ordered.push(name);
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

/// The resolved inputs handed to a builder: a map from slot name to the
/// materialized filesystem path of its content.
#[derive(Debug, Clone, Default)]
pub struct BuilderInputs {
    slots: BTreeMap<String, PathBuf>,
}

impl BuilderInputs {
    /// An empty input set.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds an input set from `slots`.
    pub fn new(slots: BTreeMap<String, PathBuf>) -> Self {
        Self { slots }
    }

    /// Whether there are no inputs.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Inserts an input under `name`.
    pub fn insert(&mut self, name: impl Into<String>, value: PathBuf) {
        self.slots.insert(name.into(), value);
    }

    /// Returns the required input `name`, or an error if it is missing.
    pub fn required(&self, name: &str) -> Result<&PathBuf, BuilderError> {
        match self.slots.get(name) {
            Some(object) => Ok(object),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    /// Returns the optional input `name`, if present.
    pub fn optional(&self, name: &str) -> Option<&PathBuf> {
        self.slots.get(name)
    }

    /// Returns the input `name`, if present.
    pub fn get(&self, name: &str) -> Option<&PathBuf> {
        self.slots.get(name)
    }

    /// Returns the input `name` only if it is an extra (not a declared slot of
    /// `spec`).
    pub fn extra<'a>(&'a self, spec: &InputSpec, name: &str) -> Option<&'a PathBuf> {
        if spec.is_reserved_input(name) {
            None
        } else {
            self.slots.get(name)
        }
    }

    /// Iterates the extra inputs (those not declared in `spec`).
    pub fn extras<'a>(
        &'a self,
        spec: &'a InputSpec,
    ) -> impl Iterator<Item = (&'a str, &'a PathBuf)> + 'a {
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
    fs_tree: FsTree,
}

impl fmt::Debug for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildContext")
            .field("temp_dir", &self.temp_dir)
            .field("runtime_backend", &self.runtime.backend())
            .finish_non_exhaustive()
    }
}

impl BuildContext {
    /// A context with a no-op logger and the host runtime, rooted at `fs_tree`
    /// — for tests and simple callers.
    pub fn with_noop_logger(temp_dir: PathBuf, fs_tree: FsTree) -> Self {
        Self {
            temp_dir,
            logger: Arc::new(NoopBuildLogger),
            cancellation: CancellationToken::new(),
            runtime: RuntimeProvider::host(),
            fs_tree,
        }
    }

    /// Returns the context with `logger` attached.
    pub fn with_logger(mut self, logger: Arc<dyn BuildLogger>) -> Self {
        self.logger = logger;
        self
    }

    /// Returns the context with `cancellation` attached.
    pub fn with_cancellation_token(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Returns the context with `runtime` attached.
    pub fn with_runtime_provider(mut self, runtime: RuntimeProvider) -> Self {
        self.runtime = runtime;
        self
    }

    /// The cancellation token for this build.
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// The runtime provider to execute runtime functions through.
    pub fn runtime(&self) -> &RuntimeProvider {
        &self.runtime
    }

    /// The store fs-tree for this build.
    pub fn fs_tree(&self) -> FsTree {
        self.fs_tree.clone()
    }

    /// Returns an error if the build has been cancelled.
    pub fn check_cancelled(&self) -> Result<(), BuilderError> {
        if self.cancellation.is_cancelled() {
            Err(BuilderError::Cancelled(
                "build cancelled by signal".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    /// Logs a builder operation. Builder events always ride inside the
    /// `running` lifecycle status; `op` names the builder-specific operation
    /// (`mkfs`, `merge`, `extract`, …).
    pub fn log_event(
        &self,
        level: BuildLogLevel,
        op: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.log_event_with_details(level, op, message, None, None, Map::new());
    }

    /// Like [`log_event`](BuildContext::log_event), but also attaches an object
    /// hash, a raw-log path, and extra structured `details`.
    pub fn log_event_with_details(
        &self,
        level: BuildLogLevel,
        op: impl Into<String>,
        message: impl Into<String>,
        object_hash: Option<ObjectHash>,
        raw_log_path: Option<PathBuf>,
        details: Map<String, Value>,
    ) {
        self.logger.log_event(BuildLogEvent {
            level,
            status: BuildStatus::Running,
            op: Some(op.into()),
            message: message.into(),
            object_hash,
            raw_log_path,
            details,
        });
    }

    /// Allocates a path for a raw log file labelled `label`.
    pub fn allocate_raw_log_path(&self, label: &str) -> Result<PathBuf, BuilderError> {
        self.logger
            .allocate_raw_log_path(label)
            .map_err(BuilderError::ExecutionFailed)
    }

    /// Writes `content` to a raw log file labelled `label`, returning its path,
    /// or `None` on failure (failures are reported as warning events).
    pub fn write_raw_log(&self, label: &str, content: &str) -> Option<PathBuf> {
        let path = match self.allocate_raw_log_path(label) {
            Ok(path) => path,
            Err(_) => return None,
        };

        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
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

/// Object-safe builder interface used by the registry and executor. Blanket-
/// implemented for every [`TypedBuilder`]; builders implement [`TypedBuilder`].
///
/// # Configuration is canonical, so identity ignores omitted defaults
///
/// A builder's configuration type must round-trip losslessly through serde:
/// deserializing a recipe's config JSON and serializing it back yields a
/// **canonical** form in which every field is present with its concrete value.
/// [`normalize_config`](Builder::normalize_config) performs that round-trip, and
/// the planner hashes the *normalized* config into the build/reuse keys (not the
/// raw recipe JSON). Two consequences the config types must uphold:
///
/// - **No direct member of a configuration object may be `Option<_>`.** A field
///   that can be left out of the recipe carries a serde default instead
///   (`#[serde(default = "…")]`), so its absence and its explicit default value
///   normalize to the same JSON — and therefore to the same key. (Nested types
///   further down may still use `Option` where `None` is a genuinely distinct
///   value, e.g. an install rule that overrides only some attributes; only the
///   object's own top-level members are constrained.)
/// - As a result, `{}`, `{"flag": <default>}`, and `{"flag": <default>, …}` all
///   produce the same build key: writing a default explicitly never forks the
///   cache from omitting it.
pub trait Builder: Send + Sync {
    /// The builder's recipe tag.
    fn tag(&self) -> &'static str;

    /// The builder's input contract.
    fn spec(&self) -> &'static InputSpec;

    /// Implementation-version token of the builder; see
    /// [`TypedBuilder::impl_version`].
    fn impl_version(&self) -> &'static str;

    /// Whether the builder's output depends on the execution architecture; see
    /// [`TypedBuilder::is_arch_dependent`].
    fn is_arch_dependent(&self) -> bool;

    /// Normalizes a recipe's raw `config` JSON into its canonical form by
    /// round-tripping it through the typed configuration (which fills defaults
    /// for omitted fields). The planner hashes this normalized value into the
    /// keys, so omitted and explicitly-default fields yield the same key. Fails
    /// if `config` does not match the builder's configuration shape.
    fn normalize_config(&self, config: Value) -> Result<Value, BuilderError>;

    /// Builds from a raw JSON `config` (deserialized internally) and `inputs`.
    fn build_erased(
        &self,
        config: Value,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError>;
}

/// The trait builders implement: a typed `Config` plus the build logic. A
/// blanket impl exposes every `TypedBuilder` as a [`Builder`].
pub trait TypedBuilder: Send + Sync {
    /// The builder's strongly-typed configuration, deserialized from the recipe.
    ///
    /// Must round-trip losslessly through serde (hence `Serialize`): the planner
    /// normalizes recipe config by deserializing then reserializing it. Keep it
    /// canonical — no top-level `Option<_>` members; use `#[serde(default = …)]`
    /// for omittable fields. See [`Builder`] for the invariant this upholds.
    type Config: DeserializeOwned + Serialize;

    /// The builder's recipe tag.
    fn tag(&self) -> &'static str;

    /// The builder's input contract.
    fn spec(&self) -> &'static InputSpec;

    /// Opaque version token of the builder's implementation, folded into the
    /// build and reuse keys. Bump it whenever the builder's output for the same
    /// config and inputs changes, so stale cached objects are not reused. Only
    /// equality matters (it is hashed, not ordered).
    fn impl_version(&self) -> &'static str;

    /// Whether the builder's output depends on the architecture it executes on.
    ///
    /// Default `false`: most builders are pure functions of their config and
    /// content-addressed inputs (any architecture difference already flows in
    /// through the input hashes). Return `true` only for builders that execute
    /// arbitrary code whose output depends on the host architecture — that
    /// architecture is otherwise an uncaptured implicit input. When `true`, the
    /// current build architecture is folded into the keys.
    fn is_arch_dependent(&self) -> bool {
        false
    }

    /// Builds from the typed `config` and resolved `inputs`, staging the output
    /// under `cx.temp_dir` and returning its path.
    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError>;
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

    fn impl_version(&self) -> &'static str {
        <T as TypedBuilder>::impl_version(self)
    }

    fn is_arch_dependent(&self) -> bool {
        <T as TypedBuilder>::is_arch_dependent(self)
    }

    fn normalize_config(&self, config: Value) -> Result<Value, BuilderError> {
        let typed: T::Config = serde_json::from_value(config).map_err(|error| {
            BuilderError::InvalidRecipe(format!("invalid builder config: {error}"))
        })?;
        serde_json::to_value(&typed).map_err(|error| {
            BuilderError::ExecutionFailed(format!("failed to normalize builder config: {error}"))
        })
    }

    fn build_erased(
        &self,
        config: Value,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError> {
        let config = serde_json::from_value(config).map_err(|error| {
            BuilderError::InvalidRecipe(format!("invalid builder config: {error}"))
        })?;
        self.build_typed(config, inputs, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::store_fs_tree;
    use bobr_runtime::runtime_provider::{RuntimeBackend, RuntimeProvider};
    use serde::Deserialize;
    use tempfile::tempdir;

    fn sample_builder_object() -> PathBuf {
        PathBuf::from("/tmp/object")
    }

    #[test]
    fn input_spec_validate_accepts_distinct_inputs() {
        static SPEC: InputSpec = InputSpec {
            required_inputs: &["rootfs", "toolchain"],
            optional_inputs: &["source"],
            allow_extra_inputs: true,
        };

        SPEC.validate_for_builder("Test").unwrap();
    }

    #[test]
    fn input_spec_validate_rejects_duplicate_required_inputs() {
        static SPEC: InputSpec = InputSpec {
            required_inputs: &["rootfs", "rootfs"],
            optional_inputs: &[],
            allow_extra_inputs: true,
        };

        assert_eq!(
            SPEC.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares duplicate required input 'rootfs'"
        );
    }

    #[test]
    fn input_spec_validate_rejects_duplicate_optional_inputs() {
        static SPEC: InputSpec = InputSpec {
            required_inputs: &[],
            optional_inputs: &["source", "source"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            SPEC.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares duplicate optional input 'source'"
        );
    }

    #[test]
    fn input_spec_validate_rejects_required_optional_overlap() {
        static SPEC: InputSpec = InputSpec {
            required_inputs: &["rootfs"],
            optional_inputs: &["rootfs"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            SPEC.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares input 'rootfs' as both required and optional"
        );
    }

    #[test]
    fn input_spec_validate_rejects_invalid_declared_input_names() {
        static REQUIRED: InputSpec = InputSpec {
            required_inputs: &["bad-name"],
            optional_inputs: &[],
            allow_extra_inputs: true,
        };
        static OPTIONAL: InputSpec = InputSpec {
            required_inputs: &[],
            optional_inputs: &["1bad"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            REQUIRED.validate_for_builder("Test").unwrap_err(),
            "builder 'Test' declares invalid required input 'bad-name': input name 'bad-name' must contain only ASCII letters, digits, and underscores"
        );
        assert_eq!(
            OPTIONAL.validate_for_builder("Test").unwrap_err(),
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

        static SPEC: InputSpec = InputSpec {
            required_inputs: &["image"],
            optional_inputs: &["base"],
            allow_extra_inputs: true,
        };

        assert_eq!(*inputs.required("script").unwrap(), object);
        assert!(inputs.optional("base").is_none());
        assert!(inputs.extra(&SPEC, "source").is_some());
        assert_eq!(inputs.extras(&SPEC).count(), 3);
    }

    #[derive(Debug)]
    struct DummyBuilder;

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
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

        fn impl_version(&self) -> &'static str {
            "test"
        }

        fn spec(&self) -> &'static InputSpec {
            &DUMMY_SPEC
        }

        fn build_typed(
            &self,
            config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<PathBuf, BuilderError> {
            assert_eq!(
                config,
                DummyConfig {
                    demo: "demo".into()
                }
            );
            assert!(inputs.is_empty());
            assert_eq!(cx.temp_dir, PathBuf::from("/tmp/tmp"));
            Ok(PathBuf::from("/tmp/out"))
        }
    }

    #[test]
    fn typed_builder_adapter_decodes_config() {
        let builder = DummyBuilder;
        let temp = tempdir().unwrap();
        let mut cx =
            BuildContext::with_noop_logger(PathBuf::from("/tmp/tmp"), store_fs_tree(temp.path()));

        let result = builder
            .build_erased(
                serde_json::json!({ "demo": "demo" }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert_eq!(result, PathBuf::from("/tmp/out"));
    }

    #[test]
    fn build_context_defaults_to_host_runtime() {
        let temp = tempdir().unwrap();
        let cx =
            BuildContext::with_noop_logger(PathBuf::from("/tmp/tmp"), store_fs_tree(temp.path()));

        assert_eq!(cx.runtime().backend(), RuntimeBackend::Host);
    }

    #[test]
    fn build_context_can_override_runtime_provider() {
        let temp = tempdir().unwrap();
        let cx =
            BuildContext::with_noop_logger(PathBuf::from("/tmp/tmp"), store_fs_tree(temp.path()))
                .with_runtime_provider(RuntimeProvider::namespace());

        assert_eq!(cx.runtime().backend(), RuntimeBackend::Namespace);
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
