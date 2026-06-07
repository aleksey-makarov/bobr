use crate::resolved_inputs::ResolvedInputs;
use bobr_store::identity::{BuildKey, compute_reuse_key};
use bobr_store::{
    PublishedBuild, ReuseInputIdentity, Store, StoreError, StoreTempQuarantineRequest,
    StoreWorkspace, WorkspaceRequest, create_workspace, load_build_handle, materialize_build,
    materialize_build_with_trusted_hash, quarantine_store_temp, recreate_store_temp_dir_force,
    remove_store_temp_dir_force, resolve_reuse_for_build,
};
use mbuild_core::{
    BuildContext, BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, Builder, BuilderError,
    BuilderRun, CancellationToken, Workspace,
};
use serde_json::Value;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
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

pub(crate) struct ExecuteBuilderNodeRequest<'a> {
    pub(crate) layout: &'a Store,
    pub(crate) builder: &'static dyn Builder,
    pub(crate) build_key: BuildKey,
    pub(crate) build_name: &'a str,
    pub(crate) run_logger: Arc<BuildRunLogger>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) config: Value,
    pub(crate) inputs: ResolvedInputs,
}

#[derive(Debug)]
pub(crate) struct ExecutedBuilderNode {
    pub(crate) published: PublishedBuild,
    pub(crate) logger: Arc<dyn BuildLogger>,
}

pub(crate) fn execute_builder_node(
    request: ExecuteBuilderNodeRequest<'_>,
) -> Result<ExecutedBuilderNode, RuntimeError> {
    let ExecuteBuilderNodeRequest {
        layout,
        builder,
        build_key,
        build_name,
        run_logger,
        cancellation,
        config,
        inputs,
    } = request;

    check_cancelled(&cancellation)?;
    let inputs_identity = inputs
        .ordered_reuse_input_identities(builder.spec())
        .map_err(map_builder_error)?;
    let workspace = create_workspace(
        layout,
        WorkspaceRequest::new(
            builder.spec().tag,
            Some(build_name.to_string()),
            build_key.to_string(),
        ),
    )
    .map(core_workspace)
    .map_err(map_store_error)?;
    let builder_run = builder.create_run(
        Some(build_name.to_string()),
        build_key.to_string(),
        workspace,
    );
    let logger = run_logger
        .bind_builder(&builder_run)
        .map_err(RuntimeError::Store)?;
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
        cleanup_temp_dir(
            builder_run.temp_dir(),
            &TempCleanupContext::new(layout, builder.spec().tag, build_key),
            logger.as_ref(),
        );
        return Ok(ExecutedBuilderNode { published, logger });
    }

    let reuse_key = compute_reuse_key(builder.spec().tag, &config, &inputs_identity)
        .map_err(map_store_error)?;
    if let Some(published) =
        resolve_reuse_for_build(layout, build_key, reuse_key).map_err(map_store_error)?
    {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "result-hit",
            "reusing existing canonical result",
        );
        cleanup_temp_dir(
            builder_run.temp_dir(),
            &TempCleanupContext::new(layout, builder.spec().tag, build_key),
            logger.as_ref(),
        );
        return Ok(ExecutedBuilderNode { published, logger });
    }

    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "executing builder",
    );
    check_cancelled(&cancellation)?;
    let cleanup = TempCleanupContext::new(layout, builder.spec().tag, build_key);
    let mut context = build_context(
        layout,
        &builder_run,
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
            cleanup_temp_dir(&context.temp_dir, &cleanup, logger.as_ref());
            runtime_error
        })?;
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_temp_dir(&context.temp_dir, &cleanup, logger.as_ref());
        return Err(error);
    }
    let published = match staged.object_hash {
        Some(object_hash) => materialize_build_with_trusted_hash(
            layout,
            build_key,
            reuse_key,
            layout.created_at(),
            inputs_identity,
            &staged.staged_path,
            object_hash,
        ),
        None => materialize_build(
            layout,
            build_key,
            reuse_key,
            layout.created_at(),
            inputs_identity,
            &staged.staged_path,
        ),
    }
    .map_err(|error| {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Error,
            "fail",
            error.to_string(),
        );
        map_store_error(error)
    });
    cleanup_temp_dir(&context.temp_dir, &cleanup, logger.as_ref());
    let published = published?;
    Ok(ExecutedBuilderNode { published, logger })
}

fn core_workspace(workspace: StoreWorkspace) -> Workspace {
    Workspace::new(
        workspace.log_dir().to_path_buf(),
        workspace.raw_log_dir().to_path_buf(),
        workspace.temp_dir().to_path_buf(),
    )
}

pub(crate) fn lookup_build_handle(
    layout: &Store,
    build_key: BuildKey,
) -> Result<Option<PublishedBuild>, RuntimeError> {
    load_build_handle(layout, build_key).map_err(map_store_error)
}

pub(crate) fn lookup_canonical_result(
    layout: &Store,
    builder_tag: &str,
    config: &Value,
    inputs: &[ReuseInputIdentity],
    build_key: BuildKey,
) -> Result<Option<PublishedBuild>, RuntimeError> {
    let reuse_key = compute_reuse_key(builder_tag, config, inputs).map_err(map_store_error)?;
    resolve_reuse_for_build(layout, build_key, reuse_key).map_err(map_store_error)
}

pub(crate) fn build_context(
    layout: &Store,
    builder_run: &BuilderRun,
    build_key: BuildKey,
    logger: Arc<dyn BuildLogger>,
    cancellation: CancellationToken,
) -> Result<BuildContext, RuntimeError> {
    let cleanup = TempCleanupContext::new(layout, builder_run.tag(), build_key);
    recreate_empty_temp_dir_with_quarantine(builder_run.temp_dir(), &cleanup, logger.as_ref())?;
    Ok(
        BuildContext::with_noop_logger(builder_run.temp_dir().to_path_buf())
            .with_logger(logger)
            .with_cancellation_token(cancellation),
    )
}

pub(crate) fn map_builder_error(error: BuilderError) -> RuntimeError {
    match error {
        BuilderError::Cancelled(message) => RuntimeError::Cancelled(message),
        other => RuntimeError::Build(other.to_string()),
    }
}

pub(crate) fn map_store_error(error: StoreError) -> RuntimeError {
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

fn recreate_empty_temp_dir_with_quarantine(
    temp_dir: &Path,
    cleanup: &TempCleanupContext,
    logger: &dyn BuildLogger,
) -> Result<(), RuntimeError> {
    if cleanup.mode == TempCleanupMode::DirectQuarantine {
        if fs::symlink_metadata(temp_dir).is_ok() && !is_empty_directory(temp_dir) {
            quarantine_temp_path(
                temp_dir,
                cleanup,
                logger,
                "stale sandbox temp dir may contain userns-owned files".to_string(),
            )
            .map_err(RuntimeError::Store)?;
        }
        return fs::create_dir_all(temp_dir).map_err(|error| {
            RuntimeError::Store(format!(
                "failed to create directory '{}': {error}",
                temp_dir.display()
            ))
        });
    }

    match recreate_store_temp_dir_force(&cleanup.layout, temp_dir) {
        Ok(()) => return Ok(()),
        Err(error) if fs::symlink_metadata(temp_dir).is_ok() => {
            quarantine_temp_path(temp_dir, cleanup, logger, error.to_string())
                .map_err(RuntimeError::Store)?;
        }
        Err(error) => return Err(RuntimeError::Store(error.to_string())),
    }

    fs::create_dir_all(temp_dir).map_err(|error| {
        RuntimeError::Store(format!(
            "failed to create directory '{}': {error}",
            temp_dir.display()
        ))
    })
}

fn is_empty_directory(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_dir() {
        return false;
    }
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempCleanupMode {
    RemoveThenQuarantine,
    DirectQuarantine,
}

#[derive(Debug)]
struct TempCleanupContext {
    layout: Store,
    builder_tag: String,
    build_key: BuildKey,
    mode: TempCleanupMode,
}

impl TempCleanupContext {
    fn new(layout: &Store, builder_tag: &str, build_key: BuildKey) -> Self {
        Self {
            layout: layout.clone(),
            builder_tag: builder_tag.to_string(),
            build_key,
            mode: cleanup_mode_for_builder(builder_tag),
        }
    }
}

fn cleanup_mode_for_builder(builder_tag: &str) -> TempCleanupMode {
    if builder_tag.eq_ignore_ascii_case("Sandbox") {
        TempCleanupMode::DirectQuarantine
    } else {
        TempCleanupMode::RemoveThenQuarantine
    }
}

fn cleanup_temp_dir(temp_dir: &Path, cleanup: &TempCleanupContext, logger: &dyn BuildLogger) {
    if cleanup.mode == TempCleanupMode::DirectQuarantine {
        if fs::symlink_metadata(temp_dir).is_ok() {
            if is_empty_directory(temp_dir) {
                if let Err(error) = remove_store_temp_dir_force(&cleanup.layout, temp_dir) {
                    log_runtime_event(
                        logger,
                        BuildLogLevel::Warn,
                        "cleanup-warning",
                        format!(
                            "failed to remove empty sandbox temp dir '{}': {error}",
                            temp_dir.display()
                        ),
                    );
                }
                return;
            }
            match quarantine_temp_path(
                temp_dir,
                cleanup,
                logger,
                "sandbox temp may contain userns-owned files".to_string(),
            ) {
                Ok(_) => return,
                Err(quarantine_error) => {
                    log_runtime_event(
                        logger,
                        BuildLogLevel::Warn,
                        "cleanup-warning",
                        format!(
                            "failed to quarantine sandbox temp dir '{}': {quarantine_error}",
                            temp_dir.display()
                        ),
                    );
                    return;
                }
            }
        }
        return;
    }

    if let Err(error) = remove_store_temp_dir_force(&cleanup.layout, temp_dir) {
        if fs::symlink_metadata(temp_dir).is_ok() {
            match quarantine_temp_path(temp_dir, cleanup, logger, error.to_string()) {
                Ok(_) => return,
                Err(quarantine_error) => {
                    log_runtime_event(
                        logger,
                        BuildLogLevel::Warn,
                        "cleanup-warning",
                        format!(
                            "failed to remove temp dir '{}': {error}; failed to quarantine it: {quarantine_error}",
                            temp_dir.display()
                        ),
                    );
                    return;
                }
            }
        }

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

fn quarantine_temp_path(
    temp_dir: &Path,
    cleanup: &TempCleanupContext,
    logger: &dyn BuildLogger,
    reason: String,
) -> Result<PathBuf, String> {
    let quarantined = quarantine_store_temp(
        &cleanup.layout,
        StoreTempQuarantineRequest {
            temp_path: temp_dir.to_path_buf(),
            builder_tag: cleanup.builder_tag.clone(),
            build_key: cleanup.build_key,
            reason: reason.clone(),
        },
    )
    .map_err(|error| error.to_string())?;
    let target = quarantined.path;
    log_runtime_event(
        logger,
        match cleanup.mode {
            TempCleanupMode::DirectQuarantine => BuildLogLevel::Info,
            TempCleanupMode::RemoveThenQuarantine => BuildLogLevel::Warn,
        },
        "temp-quarantine",
        format!(
            "moved temp dir '{}' to global quarantine '{}': {reason}",
            temp_dir.display(),
            target.display()
        ),
    );
    if let Some(metadata_error) = quarantined.metadata_error {
        log_runtime_event(
            logger,
            BuildLogLevel::Warn,
            "cleanup-warning",
            metadata_error,
        );
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_store::identity::compute_build_key;
    use bobr_store::{
        PublishOutputRequest, ReuseInputIdentity, WorkspaceRequest, create_workspace,
        list_quarantined_temps, publish_output,
    };
    use mbuild_core::{
        BuildContext, BuildLogger, BuildRunLogger, BuilderInputs, BuilderRun, BuilderSpec,
        CancellationToken, RunOptions, StagedBuildResult, TypedBuilder,
    };
    use serde::Deserialize;
    use serde_json::{Map, Value, json};
    use std::fs;
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn create_test_store(root: &Path) -> Store {
        let store_root = root.join(".mbuild");
        fs::create_dir_all(&store_root).unwrap();
        Store::create(&store_root).unwrap()
    }

    fn create_test_logger(layout: &Store) -> Arc<BuildRunLogger> {
        let locations = layout.run_log_locations();
        Arc::new(
            BuildRunLogger::new(
                locations.run_log_dir(),
                locations.created_at(),
                RunOptions::default(),
            )
            .unwrap(),
        )
    }

    fn create_test_builder_run(
        layout: &Store,
        run_logger: &Arc<BuildRunLogger>,
        tag: &str,
        name: &str,
        build_key: BuildKey,
    ) -> (BuilderRun, Arc<dyn BuildLogger>) {
        let workspace = create_workspace(
            layout,
            WorkspaceRequest::new(tag, Some(name.to_string()), build_key.to_string()),
        )
        .map(core_workspace)
        .unwrap();
        let builder_run = BuilderRun::new(
            tag.to_string(),
            Some(name.to_string()),
            build_key.to_string(),
            workspace,
        );
        let logger = run_logger.bind_builder(&builder_run).unwrap();
        (builder_run, logger)
    }

    fn workspace_metadata(root: &Path, tag: &str, name: &str) -> Value {
        let logs_root = root.join(".mbuild").join("logs");
        let mut run_dirs = fs::read_dir(&logs_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        run_dirs.sort();
        for run_dir in run_dirs {
            for entry in fs::read_dir(run_dir).unwrap() {
                let path = entry.unwrap().path();
                let meta_path = path.join("meta.json");
                if !meta_path.is_file() {
                    continue;
                }
                let metadata: Value =
                    serde_json::from_slice(&fs::read(meta_path).unwrap()).unwrap();
                if metadata["tag"] == tag && metadata["recipe_name"] == name {
                    return metadata;
                }
            }
        }
        panic!("missing workspace metadata for {tag}/{name}");
    }

    fn metadata_temp_dir(metadata: &Value) -> PathBuf {
        PathBuf::from(metadata["temp_dir"].as_str().unwrap())
    }

    fn workspace_count(root: &Path) -> usize {
        let logs_root = root.join(".mbuild").join("logs");
        fs::read_dir(&logs_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .flat_map(|run_dir| fs::read_dir(run_dir).unwrap())
            .filter(|entry| entry.as_ref().unwrap().path().join("meta.json").is_file())
            .count()
    }

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
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);

            fs::create_dir_all(cx.temp_dir.join("out")).unwrap();
            fs::write(cx.temp_dir.join("out").join("payload"), b"ok\n").unwrap();

            Ok(StagedBuildResult {
                staged_path: cx.temp_dir.join("out"),
                object_hash: None,
            })
        }
    }

    static SANDBOX_RUNTIME_TEST_SPEC: BuilderSpec = BuilderSpec {
        tag: "Sandbox",
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    #[derive(Debug)]
    struct SandboxRuntimeTestBuilder;
    static SANDBOX_RUNTIME_TEST_BUILDER: SandboxRuntimeTestBuilder = SandboxRuntimeTestBuilder;

    impl TypedBuilder for SandboxRuntimeTestBuilder {
        type Config = RuntimeTestConfig;

        fn spec(&self) -> &'static BuilderSpec {
            &SANDBOX_RUNTIME_TEST_SPEC
        }

        fn build_typed(
            &self,
            _config: Self::Config,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, mbuild_core::BuilderError> {
            assert!(inputs.is_empty());
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);

            fs::create_dir_all(cx.temp_dir.join("out")).unwrap();
            fs::write(cx.temp_dir.join("out").join("payload"), b"ok\n").unwrap();
            fs::write(cx.temp_dir.join("sandbox-scratch"), b"keep in quarantine\n").unwrap();

            Ok(StagedBuildResult {
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
            assert!(cx.temp_dir.is_dir());
            assert_eq!(fs::read_dir(&cx.temp_dir).unwrap().count(), 0);
            fs::write(cx.temp_dir.join("scratch"), b"temp\n").unwrap();

            Ok(StagedBuildResult {
                staged_path: cx.temp_dir.join("missing-output"),
                object_hash: None,
            })
        }
    }

    #[test]
    fn lookup_canonical_result_depends_on_input_object_hash() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());

        let matching_inputs = vec![ReuseInputIdentity {
            object_hash: "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
        }];
        let payload = json!({ "source": "echo hi\n", "executable": true });
        let reuse_key = compute_reuse_key("RuntimeLookupTest", &payload, &matching_inputs).unwrap();
        let build_key_for_result =
            BuildKey::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .unwrap();
        let lookup_build_key =
            BuildKey::from_str("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
                .unwrap();
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
            },
        )
        .unwrap();

        let hit = lookup_canonical_result(
            &layout,
            "RuntimeLookupTest",
            &payload,
            &matching_inputs,
            lookup_build_key,
        )
        .unwrap()
        .expect("expected canonical result hit");
        assert_eq!(hit.build.build_key, lookup_build_key);

        let mismatching_inputs = vec![ReuseInputIdentity {
            object_hash: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .parse()
                .unwrap(),
        }];
        assert!(
            lookup_canonical_result(
                &layout,
                "RuntimeLookupTest",
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
        let layout = create_test_store(temp.path());
        let logger = create_test_logger(&layout);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();

        let executed = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_TEST_BUILDER,
            build_key,
            build_name: "runtime-test",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap();

        let metadata = workspace_metadata(temp.path(), "RuntimeTest", "runtime-test");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
        assert!(executed.published.object_path.is_dir());
        assert!(executed.published.object_path.join("payload").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn build_context_quarantines_stale_temp_dir_when_recreate_fails() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let config = json!({});
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let run_logger = create_test_logger(&layout);
        let (builder_run, logger) = create_test_builder_run(
            &layout,
            &run_logger,
            "RuntimeTest",
            "runtime-test",
            build_key,
        );
        let temp_dir = builder_run.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        let stale_target = temp.path().join("missing-stale-target");
        symlink(&stale_target, &temp_dir).unwrap();

        let context = build_context(
            &layout,
            &builder_run,
            build_key,
            logger,
            CancellationToken::new(),
        )
        .unwrap();

        assert!(context.temp_dir.is_dir());
        assert!(
            !fs::symlink_metadata(&context.temp_dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let quarantined = quarantine_entries(&layout);
        assert_eq!(quarantined.len(), 1);
        assert!(
            fs::symlink_metadata(&quarantined[0])
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_quarantine_metadata(&quarantined[0], "RuntimeTest", build_key);
    }

    #[test]
    fn execute_sandbox_builder_quarantines_temp_without_removing_it() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let logger = create_test_logger(&layout);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();

        let executed = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &SANDBOX_RUNTIME_TEST_BUILDER,
            build_key,
            build_name: "sandbox-runtime-test",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap();

        let metadata = workspace_metadata(temp.path(), "Sandbox", "sandbox-runtime-test");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
        assert!(executed.published.object_path.join("payload").is_file());
        let quarantined = quarantine_entries(&layout);
        assert_eq!(quarantined.len(), 1);
        assert!(quarantined[0].join("sandbox-scratch").is_file());
        assert_quarantine_metadata(&quarantined[0], "Sandbox", build_key);
    }

    #[test]
    fn build_context_quarantines_stale_sandbox_temp_before_recreate() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let run_logger = create_test_logger(&layout);
        let config = json!({});
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();
        let (builder_run, logger) = create_test_builder_run(
            &layout,
            &run_logger,
            "Sandbox",
            "sandbox-runtime-test",
            build_key,
        );
        let temp_dir = builder_run.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("stale"), b"old\n").unwrap();

        let context = build_context(
            &layout,
            &builder_run,
            build_key,
            logger,
            CancellationToken::new(),
        )
        .unwrap();

        assert!(context.temp_dir.is_dir());
        let quarantined = quarantine_entries(&layout);
        assert_eq!(quarantined.len(), 1);
        assert!(quarantined[0].join("stale").is_file());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_failure() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let logger = create_test_logger(&layout);
        let config = Value::Object(Map::new());
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_FAIL_BUILDER,
            build_key,
            build_name: "runtime-fail",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap_err();

        assert_eq!(error.class(), "build");
        let metadata = workspace_metadata(temp.path(), "RuntimeTest", "runtime-fail");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_temp_dir_quarantines_when_remove_fails() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let run_logger = create_test_logger(&layout);
        let build_key = compute_build_key("RuntimeTest", &json!({}), &[]).unwrap();
        let (builder_run, logger) = create_test_builder_run(
            &layout,
            &run_logger,
            "RuntimeTest",
            "runtime-test",
            build_key,
        );
        let temp_dir = builder_run.temp_dir().join("stale");
        fs::create_dir_all(temp_dir.parent().unwrap()).unwrap();
        fs::write(&temp_dir, b"not a directory\n").unwrap();

        let cleanup = TempCleanupContext::new(&layout, "RuntimeTest", build_key);
        cleanup_temp_dir(&temp_dir, &cleanup, logger.as_ref());

        assert!(fs::symlink_metadata(&temp_dir).is_err());
        let quarantined = quarantine_entries(&layout);
        assert_eq!(quarantined.len(), 1);
        assert!(
            fs::symlink_metadata(&quarantined[0])
                .unwrap()
                .file_type()
                .is_file()
        );
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_materialize_failure() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let logger = create_test_logger(&layout);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_BROKEN_STAGE_BUILDER,
            build_key,
            build_name: "runtime-broken-stage",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap_err();

        assert_eq!(error.class(), "store");
        let metadata = workspace_metadata(temp.path(), "RuntimeTest", "runtime-broken-stage");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_builder_node_pre_cancelled_does_not_start_builder() {
        let temp = tempdir().unwrap();
        let layout = create_test_store(temp.path());
        let logger = create_test_logger(&layout);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_TEST_BUILDER,
            build_key,
            build_name: "runtime-test",
            run_logger: logger,
            cancellation,
            config,
            inputs,
        })
        .unwrap_err();

        assert_eq!(error.class(), "cancelled");
        assert_eq!(workspace_count(temp.path()), 0);
    }

    fn quarantine_entries(layout: &Store) -> Vec<PathBuf> {
        list_quarantined_temps(layout).unwrap()
    }

    fn assert_quarantine_metadata(path: &Path, builder_tag: &str, build_key: BuildKey) {
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let metadata_path = path.with_file_name(format!("{file_name}.json"));
        let metadata: Value = serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        assert_eq!(metadata["schema"], "bobr-quarantine-v1");
        assert_eq!(metadata["builder_tag"], builder_tag);
        assert_eq!(metadata["build_key"], build_key.to_hex());
        assert_eq!(metadata["quarantine_path"], path.display().to_string());

        let name_timestamp = file_name.split_once('-').unwrap().0;
        let (name_timestamp, collision_counter) = name_timestamp
            .split_once('.')
            .map_or((name_timestamp, None), |(timestamp, counter)| {
                (timestamp, Some(counter))
            });
        assert_eq!(name_timestamp.len(), 12);
        assert!(name_timestamp.chars().all(|ch| ch.is_ascii_digit()));
        if let Some(counter) = collision_counter {
            assert!(counter.parse::<u16>().unwrap() >= 2);
        }

        let created_at = metadata["created_at_unix_nanos"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap();
        assert!(created_at > 0);
    }
}
