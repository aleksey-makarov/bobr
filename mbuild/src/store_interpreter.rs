use crate::builders;
use crate::logging::{BuildRunLogger, RunOptions};
use crate::resolved_inputs::{ResolvedInputValue, ResolvedInputs, ResolvedObject};
use crate::runtime::{
    RuntimeError, build_to_published, execute_builder_node, log_runtime_event, map_store_error,
    to_resolved_object, validate_allowed_kind,
};
use mbuild_core::{
    Build, BuildLogLevel, BuildLogger, Builder, PublishedBuild, StoreLayout, load_build_handle,
    publish_refs,
};
use nickel_lang_core::{
    cache::{CacheHub, ImportResolver, SourcePath},
    error::{Error as NickelError, NullReporter},
    eval::{
        Closure, Environment, VirtualMachine, VmContext, cache::CacheImpl, env_add,
        value::NickelValue,
    },
    files::FileId,
    identifier::LocIdent,
    mk_app,
    serialize::{self, ExportFormat},
    term::{RecordOpKind, ToSci, UnaryOp, make as mk_term},
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

pub trait BuilderRegistry {
    fn get_builder(&self, tag: &str) -> Option<&'static dyn Builder>;
    fn supported_builder_tags(&self) -> Vec<&'static str>;
}

struct DefaultBuilderRegistry;

impl BuilderRegistry for DefaultBuilderRegistry {
    fn get_builder(&self, tag: &str) -> Option<&'static dyn Builder> {
        builders::get_builder(tag)
    }

    fn supported_builder_tags(&self) -> Vec<&'static str> {
        builders::supported_builder_tags()
    }
}

static DEFAULT_REGISTRY: DefaultBuilderRegistry = DefaultBuilderRegistry;
const STORE_LIB: &str = include_str!("../ncl/store.ncl");
const STORE_BINDING: &str = "store";

#[derive(Debug)]
pub enum StoreOutcome {
    Build(PublishedBuild),
    Unit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreRunOptions {
    pub emit_progress: bool,
}

struct NickelRuntime {
    vm_ctxt: VmContext<CacheHub, CacheImpl>,
    initial_env: Environment,
}

impl NickelRuntime {
    fn from_recipe_file(recipe_path: &Path) -> Result<(Self, NickelValue), RuntimeError> {
        let mut cache = CacheHub::new();
        let main_id = cache
            .sources
            .add_file(recipe_path, nickel_lang_core::cache::InputFormat::Nickel)
            .map_err(|error| {
                RuntimeError::RecipeLoad(format!(
                    "failed to load Nickel recipe '{}': {error}",
                    recipe_path.display()
                ))
            })?;

        Self::build(cache, main_id).map_err(|error| match error {
            RuntimeError::RecipeDiagnostic { .. } => error,
            other => RuntimeError::RecipeLoad(format!(
                "failed to evaluate Nickel recipe '{}': {other}",
                recipe_path.display()
            )),
        })
    }

    #[cfg(test)]
    fn from_source(name: &str, source: &str) -> Result<(Self, NickelValue), RuntimeError> {
        let mut cache = CacheHub::new();
        let main_id = cache
            .sources
            .add_string(SourcePath::Generated(name.to_string()), source.to_string());
        Self::build(cache, main_id).map_err(|error| match error {
            RuntimeError::RecipeDiagnostic { .. } => error,
            other => RuntimeError::RecipeLoad(format!(
                "failed to load inline Nickel test program: {other}"
            )),
        })
    }

    fn build(mut cache: CacheHub, main_id: FileId) -> Result<(Self, NickelValue), RuntimeError> {
        let store_id = cache.sources.add_string(
            SourcePath::Generated("mbuild-store".to_string()),
            STORE_LIB.to_string(),
        );
        let mut vm_ctxt = VmContext::new(cache, std::io::sink(), NullReporter {});

        Self::prepare_store_binding(&mut vm_ctxt, store_id)?;
        let prepared_main = vm_ctxt.prepare_eval(main_id).map_err(|error| {
            Self::recipe_error_from_files(vm_ctxt.import_resolver.files().clone(), error)
        })?;

        let mut initial_env = vm_ctxt.import_resolver.mk_eval_env(&mut vm_ctxt.cache);
        let store_term = vm_ctxt
            .import_resolver
            .get(store_id)
            .ok_or_else(|| {
                RuntimeError::RecipeLoad(
                    "embedded STORE API is missing from Nickel import resolver".to_string(),
                )
            })?
            .clone();
        env_add(
            &mut vm_ctxt.cache,
            &mut initial_env,
            LocIdent::from(STORE_BINDING),
            store_term,
            Environment::new(),
        );

        Ok((
            Self {
                vm_ctxt,
                initial_env,
            },
            prepared_main,
        ))
    }

    fn prepare_store_binding(
        vm_ctxt: &mut VmContext<CacheHub, CacheImpl>,
        store_id: FileId,
    ) -> Result<(), RuntimeError> {
        vm_ctxt
            .import_resolver
            .prepare_stdlib(&mut vm_ctxt.pos_table)
            .map_err(|error| {
                Self::recipe_error_from_files(vm_ctxt.import_resolver.files().clone(), error)
            })?;
        vm_ctxt
            .import_resolver
            .prepare(&mut vm_ctxt.pos_table, store_id)
            .map_err(|error| {
                Self::recipe_error_from_files(vm_ctxt.import_resolver.files().clone(), error)
            })?;
        vm_ctxt
            .import_resolver
            .closurize(&mut vm_ctxt.cache, store_id)
            .map_err(|error| {
                RuntimeError::RecipeLoad(format!(
                    "failed to closurize embedded STORE API: {error:?}"
                ))
            })?;

        let (mut slice, asts) = vm_ctxt.import_resolver.split_asts();
        asts.add_type_binding(slice.reborrow(), LocIdent::from(STORE_BINDING), store_id)
            .map_err(|error| {
                RuntimeError::RecipeLoad(format!(
                    "failed to add STORE API to Nickel type environment: {error:?}"
                ))
            })?;
        Ok(())
    }

    fn recipe_error_from_files(
        files: nickel_lang_core::files::Files,
        error: NickelError,
    ) -> RuntimeError {
        RuntimeError::RecipeDiagnostic { files, error }
    }

    fn eval_value(&mut self, value: NickelValue) -> Result<NickelValue, RuntimeError> {
        self.eval_closure(Closure::from(value))
    }

    fn eval_closure(&mut self, closure: Closure) -> Result<NickelValue, RuntimeError> {
        let files = self.vm_ctxt.import_resolver.files().clone();
        let mut vm = VirtualMachine::new_empty_env(&mut self.vm_ctxt)
            .with_initial_env(self.initial_env.clone());
        vm.eval_closure(closure)
            .map(|closure| closure.value)
            .map_err(|error| Self::recipe_error_from_files(files, error.into()))
    }

    fn eval_full_for_export(&mut self, value: NickelValue) -> Result<NickelValue, RuntimeError> {
        let files = self.vm_ctxt.import_resolver.files().clone();
        let mut vm = VirtualMachine::new_empty_env(&mut self.vm_ctxt)
            .with_initial_env(self.initial_env.clone());
        vm.eval_full_for_export_closure(Closure::from(value))
            .map_err(|error| Self::recipe_error_from_files(files, error.into()))
    }

    fn export_value_to_string(
        &mut self,
        value: NickelValue,
        format: ExportFormat,
    ) -> Result<String, RuntimeError> {
        let value = self.eval_full_for_export(value)?;
        serialize::to_string(format, &value).map_err(|error| {
            Self::recipe_error_from_files(
                self.vm_ctxt.import_resolver.files().clone(),
                NickelError::export_error(self.vm_ctxt.pos_table.clone(), error),
            )
        })
    }
}

#[derive(Debug, Deserialize)]
struct RunBuilderAction {
    name: String,
    tag: String,
    config: Value,
    inputs: Map<String, Value>,
}

pub fn run_store_recipe_in_workspace(
    workspace_root: &Path,
    recipe_path: &Path,
) -> Result<StoreOutcome, RuntimeError> {
    run_store_recipe_in_workspace_with_options(
        workspace_root,
        recipe_path,
        StoreRunOptions::default(),
    )
}

pub fn run_store_recipe_in_workspace_with_options(
    workspace_root: &Path,
    recipe_path: &Path,
    options: StoreRunOptions,
) -> Result<StoreOutcome, RuntimeError> {
    run_store_recipe_in_workspace_with_registry(
        workspace_root,
        recipe_path,
        &DEFAULT_REGISTRY,
        options,
    )
}

pub fn export_recipe_with_store(
    recipe_path: &Path,
    format: ExportFormat,
) -> Result<String, RuntimeError> {
    let (mut runtime, value) = NickelRuntime::from_recipe_file(recipe_path)?;
    runtime.export_value_to_string(value, format)
}

fn run_store_recipe_in_workspace_with_registry(
    workspace_root: &Path,
    recipe_path: &Path,
    registry: &dyn BuilderRegistry,
    options: StoreRunOptions,
) -> Result<StoreOutcome, RuntimeError> {
    if !recipe_path.exists() {
        return Err(RuntimeError::RecipeLoad(format!(
            "recipe file '{}' does not exist",
            recipe_path.display()
        )));
    }

    let layout = StoreLayout::discover(&workspace_root.join(".mbuild")).map_err(map_store_error)?;
    let logger: Arc<BuildRunLogger> = Arc::new(
        BuildRunLogger::new(
            &layout.root,
            RunOptions {
                emit_progress: options.emit_progress,
            },
        )
        .map_err(RuntimeError::Store)?,
    );
    let (mut runtime, action) = NickelRuntime::from_recipe_file(recipe_path)?;
    let action = runtime.eval_value(action)?;
    let result = interpret_store(
        &mut runtime,
        workspace_root,
        &layout,
        logger,
        registry,
        action,
    )?;
    final_store_result_to_outcome(&mut runtime, &layout, result)
}

fn final_store_result_to_outcome(
    runtime: &mut NickelRuntime,
    layout: &StoreLayout,
    value: NickelValue,
) -> Result<StoreOutcome, RuntimeError> {
    let value = runtime.eval_value(value)?;

    if value.is_null() {
        return Ok(StoreOutcome::Unit);
    }

    let build = Build::deserialize(value).map_err(|error| {
        RuntimeError::InvalidRequest(format!(
            "final STORE result must decode as Build or null: {error}"
        ))
    })?;
    Ok(StoreOutcome::Build(build_to_published(layout, build)?))
}

fn interpret_store(
    runtime: &mut NickelRuntime,
    workspace_root: &Path,
    layout: &StoreLayout,
    logger: Arc<BuildRunLogger>,
    registry: &dyn BuilderRegistry,
    value: NickelValue,
) -> Result<NickelValue, RuntimeError> {
    let value = runtime.eval_value(value)?;

    let action = value.as_enum_variant().ok_or_else(|| {
        RuntimeError::InvalidRequest(format!(
            "expected STORE action enum variant, got {:?}",
            value.type_of()
        ))
    })?;

    match action.tag.label() {
        "Return" => Ok(action.arg.clone().unwrap_or_else(NickelValue::null)),
        "Bind" => {
            let record = action.arg.clone().ok_or_else(|| {
                RuntimeError::InvalidRequest("'Bind requires an argument".to_string())
            })?;
            let action_term = record_access_term(record.clone(), "action");
            let result = interpret_store(
                runtime,
                workspace_root,
                layout,
                logger.clone(),
                registry,
                action_term,
            )?;
            let cont_term = record_access_term(record, "cont");
            let next = mk_app!(cont_term, result);
            interpret_store(runtime, workspace_root, layout, logger, registry, next)
        }
        "RunBuilder" => {
            let record = action.arg.clone().ok_or_else(|| {
                RuntimeError::InvalidRequest("'RunBuilder requires an argument".to_string())
            })?;
            let run = parse_run_builder_action(runtime, record)?;
            let builder = registry.get_builder(&run.tag).ok_or_else(|| {
                RuntimeError::UnknownBuilder(format!(
                    "unknown builder tag '{}'; supported builders: {}",
                    run.tag,
                    registry.supported_builder_tags().join(", ")
                ))
            })?;
            let inputs = resolve_action_inputs(layout, builder, run.inputs)?;
            let published = execute_builder_node(
                workspace_root,
                layout,
                builder,
                &run.name,
                logger.created_at(),
                logger.clone(),
                run.config,
                inputs,
            )?;
            publish_refs(layout, &run.name, &published).map_err(map_store_error)?;
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Info,
                "publish",
                builder.spec().tag,
                &run.name,
                published.build.build_key,
                format!(
                    "published '{}' -> {}",
                    run.name, published.build.object_hash
                ),
            );
            logger.as_ref().log_event(mbuild_core::BuildLogEvent {
                level: BuildLogLevel::Info,
                phase: "done".to_string(),
                builder: builder.spec().tag.to_string(),
                name: run.name.clone(),
                build_key: published.build.build_key,
                message: "builder node completed".to_string(),
                object_hash: Some(published.build.object_hash),
                raw_log_path: None,
                details: Map::new(),
            });
            build_to_nickel_value(&published.build)
        }
        other => Err(RuntimeError::InvalidRequest(format!(
            "unknown STORE action tag '{other}'"
        ))),
    }
}

fn parse_run_builder_action(
    runtime: &mut NickelRuntime,
    record: NickelValue,
) -> Result<RunBuilderAction, RuntimeError> {
    let inputs = eval_record_field_json(runtime, record.clone(), "inputs", "RunBuilder.inputs")?;
    let inputs = inputs.as_object().cloned().ok_or_else(|| {
        RuntimeError::InvalidRequest(
            "failed to decode RunBuilder.inputs: expected a JSON object".to_string(),
        )
    })?;

    Ok(RunBuilderAction {
        name: eval_record_field(runtime, record.clone(), "name", "RunBuilder.name as string")?,
        tag: eval_record_field(runtime, record.clone(), "tag", "RunBuilder.tag as string")?,
        config: eval_record_field_json(runtime, record, "config", "RunBuilder.config")?,
        inputs,
    })
}

fn resolve_action_inputs(
    layout: &StoreLayout,
    builder: &'static dyn Builder,
    mut raw_inputs: Map<String, Value>,
) -> Result<ResolvedInputs, RuntimeError> {
    let mut resolved = ResolvedInputs::empty();

    for slot in builder.spec().inputs {
        let value = raw_inputs.remove(slot.name).ok_or_else(|| {
            RuntimeError::InvalidRequest(format!(
                "STORE action for builder '{}' is missing input slot '{}'",
                builder.spec().tag,
                slot.name,
            ))
        })?;

        match slot.arity {
            mbuild_core::InputArity::One => {
                let object = resolve_input_build(layout, builder, slot.name, value)?;
                validate_allowed_kind(builder, slot.name, slot.allowed_kinds, &object.kind)?;
                resolved.insert(slot.name, ResolvedInputValue::One(object));
            }
            mbuild_core::InputArity::Optional => {
                if value.is_null() {
                    resolved.insert(slot.name, ResolvedInputValue::Optional(None));
                } else {
                    let object = resolve_input_build(layout, builder, slot.name, value)?;
                    validate_allowed_kind(builder, slot.name, slot.allowed_kinds, &object.kind)?;
                    resolved.insert(slot.name, ResolvedInputValue::Optional(Some(object)));
                }
            }
            mbuild_core::InputArity::Many => {
                let values = value.as_array().ok_or_else(|| {
                    RuntimeError::InvalidRequest(format!(
                        "STORE action input slot '{}' for builder '{}' must be an array of Build values",
                        slot.name,
                        builder.spec().tag,
                    ))
                })?;
                let mut objects = Vec::with_capacity(values.len());
                for value in values {
                    let object = resolve_input_build(layout, builder, slot.name, value.clone())?;
                    validate_allowed_kind(builder, slot.name, slot.allowed_kinds, &object.kind)?;
                    objects.push(object);
                }
                resolved.insert(slot.name, ResolvedInputValue::Many(objects));
            }
        }
    }

    if !raw_inputs.is_empty() {
        let mut extra = raw_inputs.keys().cloned().collect::<Vec<_>>();
        extra.sort();
        return Err(RuntimeError::InvalidRequest(format!(
            "STORE action for builder '{}' contains unexpected input slots: {}",
            builder.spec().tag,
            extra.join(", ")
        )));
    }

    Ok(resolved)
}

fn resolve_input_build(
    layout: &StoreLayout,
    builder: &'static dyn Builder,
    slot_name: &str,
    value: Value,
) -> Result<ResolvedObject, RuntimeError> {
    let supplied: Build = serde_json::from_value(value).map_err(|error| {
        RuntimeError::InvalidRequest(format!(
            "STORE action input slot '{}' for builder '{}' must contain a Build value: {error}",
            slot_name,
            builder.spec().tag,
        ))
    })?;

    let canonical = load_build_handle(layout, supplied.build_key)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            RuntimeError::Store(format!(
                "input build '{}' for builder '{}' slot '{}' is missing from store",
                supplied.build_key,
                builder.spec().tag,
                slot_name,
            ))
        })?;

    if canonical.build != supplied {
        return Err(RuntimeError::InvalidRequest(format!(
            "STORE action input build '{}' for builder '{}' slot '{}' does not match store record",
            supplied.build_key,
            builder.spec().tag,
            slot_name,
        )));
    }

    Ok(to_resolved_object(canonical))
}

fn build_to_nickel_value(build: &Build) -> Result<NickelValue, RuntimeError> {
    NickelValue::deserialize(serde_json::to_value(build).map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to serialize Build for Nickel: {error}"))
    })?)
    .map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to encode Build for Nickel: {error}"))
    })
}

fn record_access_term(record: NickelValue, field: &str) -> NickelValue {
    mk_term::op1(UnaryOp::RecordAccess(LocIdent::from(field)), record)
}

fn eval_record_field<T: DeserializeOwned>(
    runtime: &mut NickelRuntime,
    record: NickelValue,
    field: &str,
    expected: &str,
) -> Result<T, RuntimeError> {
    let value = record_access_term(record, field);
    deserialize_nickel_value(runtime, value, expected)
}

fn eval_record_field_json(
    runtime: &mut NickelRuntime,
    record: NickelValue,
    field: &str,
    expected: &str,
) -> Result<Value, RuntimeError> {
    let value = record_access_term(record, field);
    nickel_value_to_json(runtime, value, expected)
}

fn deserialize_nickel_value<T: DeserializeOwned>(
    runtime: &mut NickelRuntime,
    value: NickelValue,
    expected: &str,
) -> Result<T, RuntimeError> {
    let evaled = runtime.eval_value(value)?;
    T::deserialize(evaled).map_err(|error| {
        RuntimeError::InvalidRequest(format!("failed to decode {expected}: {error}"))
    })
}

fn nickel_value_to_json(
    runtime: &mut NickelRuntime,
    value: NickelValue,
    expected: &str,
) -> Result<Value, RuntimeError> {
    let evaled = runtime.eval_value(value)?;
    nickel_value_to_json_inner(runtime, evaled, expected)
}

fn nickel_value_to_json_inner(
    runtime: &mut NickelRuntime,
    value: NickelValue,
    expected: &str,
) -> Result<Value, RuntimeError> {
    if value.is_null() {
        return Ok(Value::Null);
    }

    if let Some(boolean) = value.as_bool() {
        return Ok(Value::Bool(boolean));
    }

    if let Some(number) = value.as_number() {
        let rendered = number.to_sci().to_string();
        let number = serde_json::Number::from_str(&rendered).map_err(|error| {
            RuntimeError::InvalidRequest(format!(
                "failed to decode {expected}: number '{rendered}' is not representable as JSON number: {error}"
            ))
        })?;
        return Ok(Value::Number(number));
    }

    if let Some(string) = value.as_string() {
        return Ok(Value::String(string.to_string()));
    }

    if let Some(array) = value.as_array() {
        let mut values = Vec::with_capacity(array.len());
        for element in array.iter() {
            values.push(nickel_value_to_json(runtime, element.clone(), expected)?);
        }
        return Ok(Value::Array(values));
    }

    if let Some(record) = value.as_record() {
        let mut object = Map::new();
        for id in record.field_names(RecordOpKind::IgnoreEmptyOpt) {
            let Some(field) = record.get(id).cloned() else {
                continue;
            };
            let Some(field_value) = field.value else {
                continue;
            };
            object.insert(
                id.into_label(),
                nickel_value_to_json(runtime, field_value, expected)?,
            );
        }
        return Ok(Value::Object(object));
    }

    if let Some(enum_variant) = value.as_enum_variant() {
        if enum_variant.arg.is_none() {
            return Ok(Value::String(enum_variant.tag.label().to_string()));
        }
    }

    Err(RuntimeError::InvalidRequest(format!(
        "failed to decode {expected}: unsupported Nickel value kind {:?}",
        value.type_of()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{
        BuildContext, BuilderError, BuilderInputs, BuilderSpec, InputArity, InputSlot,
        ProducerInfo, TypedBuilder,
    };
    use nickel_lang_core::serialize::ExportFormat;
    use std::fs;
    use tempfile::tempdir;

    #[derive(Debug)]
    struct DummyLeafBuilder;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DummyLeafConfig {
        kind: String,
        text: String,
    }

    static DUMMY_LEAF_SPEC: BuilderSpec = BuilderSpec {
        tag: "DummyLeaf",
        inputs: &[],
    };

    impl TypedBuilder for DummyLeafBuilder {
        type Config = DummyLeafConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &DUMMY_LEAF_SPEC
        }

        fn build_typed(
            &self,
            config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<mbuild_core::StagedBuildResult, BuilderError> {
            if !inputs.is_empty() {
                return Err(BuilderError::ExecutionFailed(
                    "DummyLeaf does not accept inputs".to_string(),
                ));
            }
            fs::create_dir_all(&cx.temp_root).map_err(|error| {
                BuilderError::ExecutionFailed(format!("failed to create temp dir: {error}"))
            })?;
            let staged = cx.temp_root.join("dummy-leaf.txt");
            fs::write(&staged, &config.text).map_err(|error| {
                BuilderError::ExecutionFailed(format!("failed to write dummy leaf: {error}"))
            })?;
            Ok(mbuild_core::StagedBuildResult {
                kind: config.kind,
                producer: ProducerInfo {
                    builder: "dummy-leaf".to_string(),
                },
                attrs: Map::from_iter([("echo".to_string(), Value::String(config.text))]),
                staged_path: staged,
            })
        }
    }

    #[derive(Debug)]
    struct DummyConsumerBuilder;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DummyConsumerConfig {
        kind: String,
    }

    static DUMMY_CONSUMER_INPUTS: &[InputSlot] = &[
        InputSlot {
            name: "base",
            arity: InputArity::One,
            allowed_kinds: &["dummy-leaf"],
        },
        InputSlot {
            name: "maybe",
            arity: InputArity::Optional,
            allowed_kinds: &["dummy-leaf"],
        },
        InputSlot {
            name: "others",
            arity: InputArity::Many,
            allowed_kinds: &["dummy-leaf"],
        },
    ];

    static DUMMY_CONSUMER_SPEC: BuilderSpec = BuilderSpec {
        tag: "DummyConsumer",
        inputs: DUMMY_CONSUMER_INPUTS,
    };

    impl TypedBuilder for DummyConsumerBuilder {
        type Config = DummyConsumerConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &DUMMY_CONSUMER_SPEC
        }

        fn build_typed(
            &self,
            config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<mbuild_core::StagedBuildResult, BuilderError> {
            let _base = inputs.one("base")?;
            let maybe = inputs.optional("maybe")?;
            let others = inputs.many("others")?;
            fs::create_dir_all(&cx.temp_root).map_err(|error| {
                BuilderError::ExecutionFailed(format!("failed to create temp dir: {error}"))
            })?;
            let staged = cx.temp_root.join("dummy-consumer.txt");
            fs::write(
                &staged,
                format!(
                    "base_present=true maybe={} others={}",
                    maybe.is_some(), others.len()
                ),
            )
            .map_err(|error| {
                BuilderError::ExecutionFailed(format!("failed to write dummy consumer: {error}"))
            })?;
            Ok(mbuild_core::StagedBuildResult {
                kind: config.kind,
                producer: ProducerInfo {
                    builder: "dummy-consumer".to_string(),
                },
                attrs: Map::from_iter([(
                    "has_maybe".to_string(),
                    Value::Bool(maybe.is_some()),
                )]),
                staged_path: staged,
            })
        }
    }

    static DUMMY_LEAF: DummyLeafBuilder = DummyLeafBuilder;
    static DUMMY_CONSUMER: DummyConsumerBuilder = DummyConsumerBuilder;

    struct DummyRegistry;

    impl BuilderRegistry for DummyRegistry {
        fn get_builder(&self, tag: &str) -> Option<&'static dyn Builder> {
            match tag {
                "DummyLeaf" => Some(&DUMMY_LEAF),
                "DummyConsumer" => Some(&DUMMY_CONSUMER),
                _ => None,
            }
        }

        fn supported_builder_tags(&self) -> Vec<&'static str> {
            vec!["DummyLeaf", "DummyConsumer"]
        }
    }

    fn eval_source_with_registry(
        source: &str,
        workspace_root: &Path,
        registry: &dyn BuilderRegistry,
    ) -> Result<Build, RuntimeError> {
        let layout =
            StoreLayout::discover(&workspace_root.join(".mbuild")).map_err(map_store_error)?;
        let logger = Arc::new(
            BuildRunLogger::new(
                &layout.root,
                RunOptions {
                    emit_progress: false,
                },
            )
            .map_err(RuntimeError::Store)?,
        );
        let (mut runtime, action) = NickelRuntime::from_source("<test>", source)?;
        let action = runtime.eval_value(action).map_err(|error| {
            RuntimeError::RecipeLoad(format!(
                "failed to evaluate inline Nickel test program: {error}"
            ))
        })?;
        let result = interpret_store(
            &mut runtime,
            workspace_root,
            &layout,
            logger,
            registry,
            action,
        )?;
        deserialize_nickel_value::<Build>(&mut runtime, result, "final STORE result as Build")
    }

    fn store_program(body: &str) -> String {
        body.to_string()
    }

    #[test]
    fn export_supports_plain_serializable_data_with_store_in_scope() {
        let (mut runtime, value) = NickelRuntime::from_source(
            "<export-test>",
            r#"
{
  has_fetch = std.record.has_field "fetch" store,
  has_bind = std.record.has_field "bind" store,
}
"#,
        )
        .unwrap();

        let exported = runtime
            .export_value_to_string(value, ExportFormat::Json)
            .unwrap();

        assert_eq!(
            exported,
            "{\n  \"has_bind\": true,\n  \"has_fetch\": true\n}"
        );
    }

    #[test]
    fn export_rejects_non_serializable_store_values() {
        let (mut runtime, value) = NickelRuntime::from_source(
            "<export-nonserializable>",
            r#"store.fetch "demo" { url = "https://example.invalid/demo.tar.gz", sha256 = "deadbeef" }"#,
        )
        .unwrap();

        let error = runtime
            .export_value_to_string(value, ExportFormat::Json)
            .unwrap_err();

        match error {
            RuntimeError::RecipeDiagnostic { .. } => {}
            other => panic!("expected RecipeDiagnostic, got {other:?}"),
        }
    }

    #[test]
    fn bind_passes_build_metadata_into_next_action() {
        let workspace = tempdir().unwrap();
        let program = store_program(
            r#"
store.bind (store.run_builder "first" "DummyLeaf" { kind = "dummy-leaf", text = "hello" } {}) (fun first =>
  store.run_builder "second" "DummyLeaf" { kind = "dummy-leaf", text = first.attrs.echo } {})
"#,
        );

        let result = eval_source_with_registry(&program, workspace.path(), &DummyRegistry).unwrap();

        assert_eq!(result.kind, "dummy-leaf");
        assert_eq!(result.attrs["echo"], Value::String("hello".to_string()));
        assert!(
            workspace
                .path()
                .join(".mbuild")
                .join("meta-refs")
                .join("first.json")
                .exists()
        );
        assert!(
            workspace
                .path()
                .join(".mbuild")
                .join("meta-refs")
                .join("second.json")
                .exists()
        );
    }

    #[test]
    fn run_builder_rejects_unknown_tag() {
        let workspace = tempdir().unwrap();
        let program = store_program(r#"store.run_builder "missing" "NoSuchBuilder" {} {}"#);

        let error =
            eval_source_with_registry(&program, workspace.path(), &DummyRegistry).unwrap_err();

        assert!(matches!(error, RuntimeError::UnknownBuilder(_)));
    }

    #[test]
    fn run_builder_validates_allowed_kinds() {
        let workspace = tempdir().unwrap();
        let program = store_program(
            r#"
store.bind (store.run_builder "leaf" "DummyLeaf" { kind = "other-kind", text = "hello" } {}) (fun leaf =>
  store.run_builder "consumer" "DummyConsumer" { kind = "dummy-output" } {
    base = leaf,
    maybe = null,
    others = [],
  })
"#,
        );

        let error =
            eval_source_with_registry(&program, workspace.path(), &DummyRegistry).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidRequest(_)));
        assert!(error.to_string().contains("rejects kind 'other-kind'"));
    }

    #[test]
    fn run_builder_rejects_missing_or_extra_slots() {
        let workspace = tempdir().unwrap();
        let missing_program = store_program(
            r#"
store.bind (store.run_builder "leaf" "DummyLeaf" { kind = "dummy-leaf", text = "hello" } {}) (fun leaf =>
  store.run_builder "consumer" "DummyConsumer" { kind = "dummy-output" } {
    base = leaf,
    others = [],
  })
"#,
        );
        let missing_error =
            eval_source_with_registry(&missing_program, workspace.path(), &DummyRegistry)
                .unwrap_err();
        assert!(
            missing_error
                .to_string()
                .contains("missing input slot 'maybe'")
        );

        let extra_workspace = tempdir().unwrap();
        let extra_program = store_program(
            r#"
store.bind (store.run_builder "leaf" "DummyLeaf" { kind = "dummy-leaf", text = "hello" } {}) (fun leaf =>
  store.run_builder "consumer" "DummyConsumer" { kind = "dummy-output" } {
    base = leaf,
    maybe = null,
    others = [],
    unexpected = leaf,
  })
"#,
        );
        let extra_error =
            eval_source_with_registry(&extra_program, extra_workspace.path(), &DummyRegistry)
                .unwrap_err();
        assert!(
            extra_error
                .to_string()
                .contains("unexpected input slots: unexpected")
        );
    }
}
