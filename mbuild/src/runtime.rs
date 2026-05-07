use crate::logging::BuildRunLogger;
use crate::resolved_inputs::ResolvedInputs;
use mbuild_core::{
    Build, BuildContext, BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, Builder,
    BuilderError, CancellationToken, CasError, PublishedBuild, ResultInputIdentity, StoreLayout,
    compute_reuse_key, fsutil, load_build_handle, load_reuse_record, materialize_build,
    object_path,
};
use serde_json::Value;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
pub enum RuntimeError {
    InvalidRequest(String),
    UnknownBuilder(String),
    RecipeLoad(String),
    Cancelled(String),
    Build(String),
    Store(String),
}

impl RuntimeError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid-request",
            Self::UnknownBuilder(_) => "unknown-builder",
            Self::RecipeLoad(_) => "recipe-load",
            Self::Cancelled(_) => "cancelled",
            Self::Build(_) => "build",
            Self::Store(_) => "store",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::UnknownBuilder(message)
            | Self::RecipeLoad(message)
            | Self::Cancelled(message)
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
    layout: &StoreLayout,
    builder: &'static dyn Builder,
    build_key: BuildKey,
    build_name: &str,
    created_at: &str,
    run_logger: Arc<BuildRunLogger>,
    cancellation: CancellationToken,
    config: Value,
    inputs: ResolvedInputs,
) -> Result<PublishedBuild, RuntimeError> {
    check_cancelled(&cancellation)?;
    let inputs_identity = inputs
        .ordered_input_identities(builder.spec())
        .map_err(map_builder_error)?;
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

    let reuse_key = compute_reuse_key(builder.spec().tag, &config, &inputs_identity)
        .map_err(map_store_error)?;
    if let Some(result) = load_reuse_record(layout, reuse_key).map_err(map_store_error)? {
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
                result.result_id,
                object_path.display()
            )));
        }
        mbuild_core::store_build_handle_ref(layout, build_key, result.result_id)
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
                result_id: result.result_id,
                object_hash: result.object_hash,
                meta_hash: result.meta_hash,
                created_at: result.created_at.clone(),
                meta: result.meta.clone(),
            },
            reuse_key,
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
    check_cancelled(&cancellation)?;
    let mut context = build_context(
        layout,
        builder.spec().tag,
        build_key,
        logger.clone(),
        cancellation.clone(),
    )?;
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
            let runtime_error = map_builder_error(error);
            cleanup_temp_dir(&context.temp_dir, logger.as_ref());
            runtime_error
        })?;
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_temp_dir(&context.temp_dir, logger.as_ref());
        return Err(error);
    }
    let published = materialize_build(
        layout,
        build_key,
        reuse_key,
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
    });
    cleanup_temp_dir(&context.temp_dir, logger.as_ref());
    let published = published?;
    Ok(published)
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
    let reuse_key = compute_reuse_key(builder_tag, config, inputs).map_err(map_store_error)?;
    let Some(result) = load_reuse_record(layout, reuse_key).map_err(map_store_error)? else {
        return Ok(None);
    };
    let object_path = object_path(layout, result.object_hash);
    if !object_path.exists() {
        return Err(RuntimeError::Store(format!(
            "result '{}' points to missing object '{}'",
            result.result_id,
            object_path.display()
        )));
    }
    mbuild_core::store_build_handle_ref(layout, build_key, result.result_id)
        .map_err(map_store_error)?;
    Ok(Some(PublishedBuild {
        build: Build {
            build_key,
            result_id: result.result_id,
            object_hash: result.object_hash,
            meta_hash: result.meta_hash,
            created_at: result.created_at.clone(),
            meta: result.meta.clone(),
        },
        reuse_key,
        result,
        object_path,
    }))
}

pub(crate) fn build_context(
    layout: &StoreLayout,
    builder_tag: &str,
    build_key: BuildKey,
    logger: Arc<dyn BuildLogger>,
    cancellation: CancellationToken,
) -> Result<BuildContext, RuntimeError> {
    let state_dir = layout
        .root
        .join("builder-state")
        .join(builder_tag.to_ascii_lowercase());
    let temp_dir = state_dir.join("tmp").join(build_key.to_hex());
    fs::create_dir_all(&state_dir).map_err(|error| {
        RuntimeError::Store(format!(
            "failed to create builder state directory '{}': {error}",
            state_dir.display()
        ))
    })?;
    fsutil::recreate_empty_dir_force(&temp_dir)
        .map_err(|error| RuntimeError::Store(error.to_string()))?;
    Ok(BuildContext::with_noop_logger(state_dir, temp_dir)
        .with_logger(logger)
        .with_cancellation_token(cancellation))
}

pub(crate) fn map_builder_error(error: BuilderError) -> RuntimeError {
    match error {
        BuilderError::Cancelled(message) => RuntimeError::Cancelled(message),
        other => RuntimeError::Build(other.to_string()),
    }
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

pub(crate) fn check_cancelled(cancellation: &CancellationToken) -> Result<(), RuntimeError> {
    if cancellation.is_cancelled() {
        Err(RuntimeError::Cancelled(
            "build cancelled by signal".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn cleanup_temp_dir(temp_dir: &Path, logger: &dyn BuildLogger) {
    if let Err(error) = fsutil::remove_dir_force(temp_dir) {
        log_runtime_event(
            logger,
            BuildLogLevel::Warn,
            "cleanup-warning",
            format!(
                "failed to remove temp dir '{}': {error}",
                temp_dir.display()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::{BuildRunLogger, RunOptions};
    use mbuild_core::{
        BuildContext, BuilderInputs, BuilderSpec, CancellationToken, PublishOutputRequest,
        ResultInputIdentity, StagedBuildResult, TypedBuilder, compute_build_key, publish_output,
    };
    use serde::Deserialize;
    use serde_json::{Map, Value, json};
    use std::fs;
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RuntimeTestConfig {}

    static RUNTIME_TEST_SPEC: BuilderSpec = BuilderSpec {
        tag: "RuntimeTest",
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    #[derive(Debug)]
    struct RuntimeTestBuilder;
    static RUNTIME_TEST_BUILDER: RuntimeTestBuilder = RuntimeTestBuilder;

    impl TypedBuilder for RuntimeTestBuilder {
        type Config = RuntimeTestConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &RUNTIME_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_core::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.state_dir.is_dir());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);

            fs::create_dir_all(cx.temp_dir.join("out")).unwrap();
            fs::write(cx.temp_dir.join("out").join("payload"), b"ok\n").unwrap();

            Ok(StagedBuildResult {
                meta: Map::new(),
                staged_path: cx.temp_dir.join("out"),
                object_hash: None,
            })
        }
    }

    #[derive(Debug)]
    struct RuntimeFailBuilder;
    static RUNTIME_FAIL_BUILDER: RuntimeFailBuilder = RuntimeFailBuilder;

    impl TypedBuilder for RuntimeFailBuilder {
        type Config = RuntimeTestConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &RUNTIME_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_core::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.state_dir.is_dir());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);
            fs::write(cx.temp_dir.join("scratch"), b"temp\n").unwrap();
            Err(mbuild_core::BuilderError::ExecutionFailed(
                "intentional failure".to_string(),
            ))
        }
    }

    #[derive(Debug)]
    struct RuntimeBrokenStageBuilder;
    static RUNTIME_BROKEN_STAGE_BUILDER: RuntimeBrokenStageBuilder = RuntimeBrokenStageBuilder;

    impl TypedBuilder for RuntimeBrokenStageBuilder {
        type Config = RuntimeTestConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &RUNTIME_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_core::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.state_dir.is_dir());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);
            fs::write(cx.temp_dir.join("scratch"), b"temp\n").unwrap();

            Ok(StagedBuildResult {
                meta: Map::new(),
                staged_path: cx.temp_dir.join("missing-output"),
                object_hash: None,
            })
        }
    }

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
        let payload = json!({ "source": "echo hi\n", "executable": true });
        let reuse_key = compute_reuse_key("Text", &payload, &matching_inputs).unwrap();
        let build_key_for_result =
            BuildKey::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .unwrap();
        let lookup_build_key =
            BuildKey::from_str("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
                .unwrap();
        let meta = Map::new();
        let expected_meta_hash = mbuild_core::compute_meta_hash(&meta).unwrap();

        let stage = temp.path().join("script.sh");
        fs::write(&stage, b"echo hi\n").unwrap();
        publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "script".to_string(),
                build_key: build_key_for_result,
                reuse_key,
                created_at: "2026-04-05T12:00:00.000000000Z".to_string(),
                staged_path: stage,
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

    #[test]
    fn execute_builder_node_prepares_dirs_and_cleans_temp_on_success() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let logger = Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let state_dir = layout.root.join("builder-state").join("runtimetest");
        let temp_dir = state_dir.join("tmp").join(build_key.to_hex());
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("stale"), b"old\n").unwrap();

        let published = execute_builder_node(
            &layout,
            &RUNTIME_TEST_BUILDER,
            build_key,
            "runtime-test",
            "2026-04-05T12:00:00.000000000Z",
            logger,
            CancellationToken::new(),
            config,
            inputs,
        )
        .unwrap();

        assert!(state_dir.is_dir());
        assert!(!temp_dir.exists());
        assert!(published.build.meta.is_empty());
        assert!(published.object_path.is_dir());
        assert!(published.object_path.join("payload").is_file());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_failure() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let logger = Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let config = Value::Object(Map::new());
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let temp_dir = layout
            .root
            .join("builder-state")
            .join("runtimetest")
            .join("tmp")
            .join(build_key.to_hex());
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("stale"), b"old\n").unwrap();

        let error = execute_builder_node(
            &layout,
            &RUNTIME_FAIL_BUILDER,
            build_key,
            "runtime-fail",
            "2026-04-05T12:00:00.000000000Z",
            logger,
            CancellationToken::new(),
            config,
            inputs,
        )
        .unwrap_err();

        assert_eq!(error.class(), "build");
        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_materialize_failure() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let logger = Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let temp_dir = layout
            .root
            .join("builder-state")
            .join("runtimetest")
            .join("tmp")
            .join(build_key.to_hex());

        let error = execute_builder_node(
            &layout,
            &RUNTIME_BROKEN_STAGE_BUILDER,
            build_key,
            "runtime-broken-stage",
            "2026-04-05T12:00:00.000000000Z",
            logger,
            CancellationToken::new(),
            config,
            inputs,
        )
        .unwrap_err();

        assert_eq!(error.class(), "store");
        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_builder_node_pre_cancelled_does_not_start_builder() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let logger = Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = execute_builder_node(
            &layout,
            &RUNTIME_TEST_BUILDER,
            build_key,
            "runtime-test",
            "2026-04-05T12:00:00.000000000Z",
            logger,
            cancellation,
            config,
            inputs,
        )
        .unwrap_err();

        assert_eq!(error.class(), "cancelled");
        assert!(
            !layout
                .root
                .join("builder-state")
                .join("runtimetest")
                .join("tmp")
                .join(build_key.to_hex())
                .exists()
        );
    }
}
