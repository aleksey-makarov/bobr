use crate::logging::BuildRunLogger;
use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use mbuild_core::{
    Build, BuildContext, BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, Builder,
    BuilderError, CasError, PublishedBuild, ResultInputIdentity, StoreLayout, compute_build_key,
    compute_result_key, load_build_handle, materialize_build, object_path,
};
use serde_json::Value;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
pub enum RuntimeError {
    InvalidRequest(String),
    UnknownBuilder(String),
    RecipeLoad(String),
    Build(String),
    Store(String),
}

impl RuntimeError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid-request",
            Self::UnknownBuilder(_) => "unknown-builder",
            Self::RecipeLoad(_) => "recipe-load",
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
    run_logger: Arc<BuildRunLogger>,
    config: Value,
    inputs: ResolvedInputs,
) -> Result<PublishedBuild, RuntimeError> {
    let input_build_keys = inputs
        .ordered_build_keys(builder.spec())
        .map_err(map_builder_error)?;
    let inputs_identity = inputs
        .ordered_input_identities(builder.spec())
        .map_err(map_builder_error)?;
    let build_key = compute_build_key(builder.spec().tag, &config, &input_build_keys)
        .map_err(map_store_error)?;
    let logger = run_logger.bind_node(builder.spec().tag, build_name, build_key);
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting builder node",
    );

    if let Some(published) = load_build_handle(layout, build_key).map_err(map_store_error)? {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "cache-hit",
            "reusing existing build ref",
        );
        return Ok(published);
    }

    let result_key = compute_result_key(builder.spec().tag, &config, &inputs_identity)
        .map_err(map_store_error)?;
    if let Some(result) =
        mbuild_core::load_result_record(layout, result_key).map_err(map_store_error)?
    {
        let object_path = object_path(layout, result.object_hash);
        if !object_path.exists() {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                format!(
                    "result points to missing object '{}'",
                    object_path.display()
                ),
            );
            return Err(RuntimeError::Store(format!(
                "result '{}' points to missing object '{}'",
                result_key,
                object_path.display()
            )));
        }
        mbuild_core::store_build_handle_ref(layout, build_key, result_key)
            .map_err(map_store_error)?;
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "result-hit",
            "reusing existing canonical result",
        );
        return Ok(PublishedBuild {
            build: Build {
                build_key,
                object_hash: result.object_hash,
                meta_hash: result.meta_hash,
                created_at: result.created_at.clone(),
                kind: result.kind.clone(),
                producer: result.producer.clone(),
                meta: result.meta.clone(),
            },
            result,
            object_path,
        });
    }

    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "executing builder",
    );
    let mut context = build_context(
        workspace_root,
        layout,
        builder.spec().tag,
        build_key,
        logger.clone(),
    );
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "run",
        "running builder implementation",
    );
    let staged = builder
        .build_erased(config, inputs.into_builder_inputs(), &mut context)
        .map_err(|error| {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                error.to_string(),
            );
            map_builder_error(error)
        })?;
    let published = materialize_build(
        layout,
        build_key,
        result_key,
        created_at,
        inputs_identity,
        staged,
    )
    .map_err(|error| {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Error,
            "fail",
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
    load_build_handle(layout, build.build_key)
        .map_err(map_store_error)?
        .ok_or_else(|| {
            RuntimeError::Store(format!("build '{}' is missing from store", build.build_key))
        })
}

pub(crate) fn lookup_build_handle(
    layout: &StoreLayout,
    build_key: BuildKey,
) -> Result<Option<PublishedBuild>, RuntimeError> {
    load_build_handle(layout, build_key).map_err(map_store_error)
}

pub(crate) fn lookup_canonical_result(
    layout: &StoreLayout,
    builder_tag: &str,
    config: &Value,
    inputs: &[ResultInputIdentity],
    build_key: BuildKey,
) -> Result<Option<PublishedBuild>, RuntimeError> {
    let result_key = compute_result_key(builder_tag, config, inputs).map_err(map_store_error)?;
    let Some(result) =
        mbuild_core::load_result_record(layout, result_key).map_err(map_store_error)?
    else {
        return Ok(None);
    };
    let object_path = object_path(layout, result.object_hash);
    if !object_path.exists() {
        return Err(RuntimeError::Store(format!(
            "result '{}' points to missing object '{}'",
            result_key,
            object_path.display()
        )));
    }
    mbuild_core::store_build_handle_ref(layout, build_key, result_key).map_err(map_store_error)?;
    Ok(Some(PublishedBuild {
        build: Build {
            build_key,
            object_hash: result.object_hash,
            meta_hash: result.meta_hash,
            created_at: result.created_at.clone(),
            kind: result.kind.clone(),
            producer: result.producer.clone(),
            meta: result.meta.clone(),
        },
        result,
        object_path,
    }))
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

pub(crate) fn to_resolved_dependency(published: PublishedBuild) -> ResolvedDependency {
    ResolvedDependency {
        object_hash: published.build.object_hash,
        meta_hash: published.build.meta_hash,
        build_key: published.build.build_key,
        kind: published.build.kind,
        object_path: published.object_path,
    }
}

pub(crate) fn build_context(
    workspace_root: &Path,
    layout: &StoreLayout,
    builder_tag: &str,
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
    message: impl Into<String>,
) {
    logger.log_event(BuildLogEvent {
        level,
        phase: phase.to_string(),
        message: message.into(),
        object_hash: None,
        raw_log_path: None,
        details: serde_json::Map::new(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{PublishOutputRequest, ResultInputIdentity, publish_output};
    use serde_json::{Map, json};
    use std::fs;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn lookup_canonical_result_depends_on_input_meta_hash() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let matching_inputs = vec![ResultInputIdentity {
            object_hash: "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
            meta_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .unwrap(),
        }];
        let payload = json!({ "kind": "build-script", "source": "echo hi\n" });
        let result_key = compute_result_key("Text", &payload, &matching_inputs).unwrap();
        let build_key_for_result =
            BuildKey::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .unwrap();
        let lookup_build_key =
            BuildKey::from_str("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
                .unwrap();
        let meta = Map::from_iter([("source_bytes".to_string(), serde_json::Value::from(8))]);
        let expected_meta_hash = mbuild_core::compute_meta_hash(&meta).unwrap();

        let stage = temp.path().join("script.sh");
        fs::write(&stage, b"echo hi\n").unwrap();
        publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "script".to_string(),
                build_key: build_key_for_result,
                result_key,
                created_at: "2026-04-05T12:00:00.000000000Z".to_string(),
                staged_path: stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                inputs: matching_inputs.clone(),
                meta,
            },
        )
        .unwrap();

        let hit = lookup_canonical_result(
            &layout,
            "Text",
            &payload,
            &matching_inputs,
            lookup_build_key,
        )
        .unwrap()
        .expect("expected canonical result hit");
        assert_eq!(hit.build.meta_hash, expected_meta_hash);

        let mismatching_inputs = vec![ResultInputIdentity {
            object_hash: matching_inputs[0].object_hash,
            meta_hash: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .parse()
                .unwrap(),
        }];
        assert!(
            lookup_canonical_result(
                &layout,
                "Text",
                &payload,
                &mismatching_inputs,
                lookup_build_key,
            )
            .unwrap()
            .is_none()
        );
    }
}
