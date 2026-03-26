use mbuild_core::{
    Build, BuildContext, BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, Builder,
    BuilderError, CasError, InputArity, PublishedBuild, ResolvedInputs, ResolvedObject,
    StoreLayout, compute_build_key, compute_result_key, load_published_build, materialize_build,
    object_path,
};
use nickel_lang_core::{error::Error as NickelError, files::Files as NickelFiles};
use fsobj_hash::ObjectHash;
use serde_json::Value;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
pub enum RuntimeError {
    InvalidRequest(String),
    UnknownBuilder(String),
    RecipeLoad(String),
    RecipeDiagnostic {
        files: NickelFiles,
        error: NickelError,
    },
    Build(String),
    Store(String),
}

impl RuntimeError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid-request",
            Self::UnknownBuilder(_) => "unknown-builder",
            Self::RecipeLoad(_) => "recipe-load",
            Self::RecipeDiagnostic { .. } => "recipe-diagnostic",
            Self::Build(_) => "build",
            Self::Store(_) => "store",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::UnknownBuilder(message)
            | Self::RecipeLoad(message)
            | Self::Build(message)
            | Self::Store(message) => message,
            Self::RecipeDiagnostic { .. } => "Nickel recipe error",
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for RuntimeError {}

pub(crate) fn execute_builder_node(
    workspace_root: &Path,
    layout: &StoreLayout,
    builder: &'static dyn Builder,
    build_name: &str,
    created_at: &str,
    logger: Arc<dyn BuildLogger>,
    config: Value,
    inputs: ResolvedInputs,
) -> Result<PublishedBuild, RuntimeError> {
    let input_build_keys = collect_input_build_keys(builder, &inputs)?;
    let input_object_hashes = collect_input_object_hashes(builder, &inputs)?;
    let build_key = compute_build_key(builder.spec().tag, &config, &input_build_keys)
        .map_err(map_store_error)?;
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        builder.spec().tag,
        build_name,
        build_key,
        "starting builder node",
    );

    if let Some(published) = load_published_build(layout, build_key).map_err(map_store_error)? {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "cache-hit",
            builder.spec().tag,
            build_name,
            build_key,
            "reusing existing build ref",
        );
        return Ok(published);
    }

    let result_key = compute_result_key(builder.spec().tag, &config, &input_object_hashes)
        .map_err(map_store_error)?;
    if let Some(result) = mbuild_core::load_result_record(layout, result_key).map_err(map_store_error)?
    {
        let object_path = object_path(layout, result.object_hash);
        if !object_path.exists() {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                builder.spec().tag,
                build_name,
                build_key,
                format!("result points to missing object '{}'", object_path.display()),
            );
            return Err(RuntimeError::Store(format!(
                "result '{}' points to missing object '{}'",
                result_key,
                object_path.display()
            )));
        }
        mbuild_core::store_build_ref(layout, build_key, result_key).map_err(map_store_error)?;
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "result-hit",
            builder.spec().tag,
            build_name,
            build_key,
            "reusing existing canonical result",
        );
        return Ok(PublishedBuild {
            build: Build {
                build_key,
                object_hash: result.object_hash,
                created_at: result.created_at.clone(),
                kind: result.kind.clone(),
                producer: result.producer.clone(),
                attrs: result.attrs.clone(),
            },
            result,
            object_path,
        });
    }

    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        builder.spec().tag,
        build_name,
        build_key,
        "executing builder",
    );
    let mut context = build_context(
        workspace_root,
        layout,
        builder.spec().tag,
        build_name,
        build_key,
        logger.clone(),
    );
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "run",
        builder.spec().tag,
        build_name,
        build_key,
        "running builder implementation",
    );
    let staged = builder
        .build_erased(config, inputs, &mut context)
        .map_err(|error| {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                builder.spec().tag,
                build_name,
                build_key,
                error.to_string(),
            );
            map_builder_error(error)
        })?;
    let published = materialize_build(
        layout,
        build_key,
        result_key,
        created_at,
        input_object_hashes,
        staged,
    )
    .map_err(|error| {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Error,
            "fail",
            builder.spec().tag,
            build_name,
            build_key,
            error.to_string(),
        );
        map_store_error(error)
    })?;
    Ok(published)
}

pub(crate) fn build_to_published(
    layout: &StoreLayout,
    build: Build,
) -> Result<PublishedBuild, RuntimeError> {
    load_published_build(layout, build.build_key)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            RuntimeError::Store(format!(
                "build '{}' is missing from store",
                build.build_key
            ))
        })
}

pub(crate) fn collect_input_build_keys(
    builder: &dyn Builder,
    inputs: &ResolvedInputs,
) -> Result<Vec<BuildKey>, RuntimeError> {
    let mut ordered = Vec::new();

    for slot in builder.spec().inputs {
        match slot.arity {
            InputArity::One => {
                ordered.push(inputs.one(slot.name).map_err(map_builder_error)?.build_key)
            }
            InputArity::Optional => {
                if let Some(object) = inputs.optional(slot.name).map_err(map_builder_error)? {
                    ordered.push(object.build_key);
                }
            }
            InputArity::Many => {
                ordered.extend(
                    inputs
                        .many(slot.name)
                        .map_err(map_builder_error)?
                        .iter()
                        .map(|object| object.build_key),
                );
            }
        }
    }

    Ok(ordered)
}

pub(crate) fn collect_input_object_hashes(
    builder: &dyn Builder,
    inputs: &ResolvedInputs,
) -> Result<Vec<ObjectHash>, RuntimeError> {
    let mut ordered = Vec::new();

    for slot in builder.spec().inputs {
        match slot.arity {
            InputArity::One => {
                ordered.push(inputs.one(slot.name).map_err(map_builder_error)?.object_hash)
            }
            InputArity::Optional => {
                if let Some(object) = inputs.optional(slot.name).map_err(map_builder_error)? {
                    ordered.push(object.object_hash);
                }
            }
            InputArity::Many => {
                ordered.extend(
                    inputs
                        .many(slot.name)
                        .map_err(map_builder_error)?
                        .iter()
                        .map(|object| object.object_hash),
                );
            }
        }
    }

    Ok(ordered)
}

pub(crate) fn validate_allowed_kind(
    builder: &dyn Builder,
    slot_name: &str,
    allowed_kinds: &[&str],
    actual_kind: &str,
) -> Result<(), RuntimeError> {
    if allowed_kinds.is_empty() || allowed_kinds.iter().any(|kind| *kind == actual_kind) {
        return Ok(());
    }
    Err(RuntimeError::InvalidRequest(format!(
        "builder '{}' input slot '{}' rejects kind '{}'; allowed kinds: {}",
        builder.spec().tag,
        slot_name,
        actual_kind,
        allowed_kinds.join(", ")
    )))
}

pub(crate) fn to_resolved_object(published: PublishedBuild) -> ResolvedObject {
    ResolvedObject {
        object_hash: published.build.object_hash,
        build_key: published.build.build_key,
        result_key: published.result.result_key,
        kind: published.build.kind,
        attrs: published.build.attrs,
        object_path: published.object_path,
    }
}

pub(crate) fn build_context(
    workspace_root: &Path,
    layout: &StoreLayout,
    builder_tag: &str,
    build_name: &str,
    build_key: BuildKey,
    logger: Arc<dyn BuildLogger>,
) -> BuildContext {
    let builder_root = layout
        .root
        .join("builder-state")
        .join(builder_tag.to_ascii_lowercase());
    BuildContext::with_noop_logger(
        workspace_root.to_path_buf(),
        builder_root.clone(),
        builder_root.join("tmp").join(build_key.to_hex()),
        build_key,
        builder_tag,
        build_name,
    )
    .with_logger(logger)
}

pub(crate) fn map_builder_error(error: BuilderError) -> RuntimeError {
    RuntimeError::Build(error.to_string())
}

pub(crate) fn map_store_error(error: CasError) -> RuntimeError {
    RuntimeError::Store(error.to_string())
}

pub(crate) fn log_runtime_event(
    logger: &dyn BuildLogger,
    level: BuildLogLevel,
    phase: &str,
    builder_tag: &str,
    build_name: &str,
    build_key: BuildKey,
    message: impl Into<String>,
) {
    logger.log_event(BuildLogEvent {
        level,
        phase: phase.to_string(),
        builder: builder_tag.to_string(),
        name: build_name.to_string(),
        build_key,
        message: message.into(),
        object_hash: None,
        raw_log_path: None,
        details: serde_json::Map::new(),
    });
}
