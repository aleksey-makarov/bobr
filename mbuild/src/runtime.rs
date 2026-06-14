use crate::resolved_inputs::ResolvedInputs;
use bobr_store::{
    PublishedBuild, Store, StoreError, StoreTempQuarantineRequest, StoreWorkspace,
    create_workspace, load_build_handle, materialize_build, materialize_build_with_trusted_hash,
    quarantine_store_temp, recreate_store_temp_dir_force, remove_store_temp_dir_force,
    resolve_reuse_for_build,
};
use mbuild_core::{
    BuildContext, BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, BuildRunLogger, Builder,
    BuilderError, BuilderRun, BuilderRunInit, CancellationToken, IdentityError, NoopBuildLogger,
    Workspace, compute_reuse_key,
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
    pub(crate) store: &'a Store,
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
        store,
        builder,
        build_key,
        build_name,
        run_logger,
        cancellation,
        config,
        inputs,
    } = request;

    check_cancelled(&cancellation)?;
    let input_hashes = inputs
        .ordered_reuse_input_hashes(builder.spec())
        .map_err(map_builder_error)?;
    // Resolve the caches before building a workspace: a hit needs no
    // workspace, logger, or temp dir, and is left silent (NoopBuildLogger).
    if let Some(published) = load_build_handle(store, build_key).map_err(map_store_error)? {
        return Ok(ExecutedBuilderNode {
            published,
            logger: Arc::new(NoopBuildLogger),
        });
    }
    let reuse_key =
        compute_reuse_key(builder.tag(), &config, &input_hashes).map_err(map_identity_error)?;
    if let Some(published) =
        resolve_reuse_for_build(store, build_key, reuse_key).map_err(map_store_error)?
    {
        return Ok(ExecutedBuilderNode {
            published,
            logger: Arc::new(NoopBuildLogger),
        });
    }

    // Miss: create the workspace and run the builder.
    let workspace = create_workspace(
        store,
        builder.tag(),
        Some(build_name.to_string()),
        build_key.to_string(),
    )
    .map(core_workspace)
    .map_err(map_store_error)?;
    let builder_run = builder.create_object(BuilderRunInit {
        recipe_name: Some(build_name.to_string()),
        build_key: build_key.to_string(),
        workspace,
    });
    // Owns the temp dir from here on: every return path (bind error below, and
    // panics) cleans it via Drop.
    let mut temp_guard = TempDirGuard::for_builder(
        store,
        builder.tag(),
        build_key,
        builder_run.temp_dir().to_path_buf(),
    );
    let logger = run_logger
        .bind_builder(&builder_run)
        .map_err(RuntimeError::Store)?;
    temp_guard.set_logger(logger.clone());
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting builder node",
    );
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "executing builder",
    );
    check_cancelled(&cancellation)?;
    let mut context = build_context(
        store,
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
            map_builder_error(error)
        })?;
    check_cancelled(&cancellation)?;
    let published = match staged.object_hash {
        Some(object_hash) => materialize_build_with_trusted_hash(
            store,
            build_key,
            reuse_key,
            input_hashes,
            &staged.staged_path,
            object_hash,
        ),
        None => materialize_build(
            store,
            build_key,
            reuse_key,
            input_hashes,
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
    })?;
    Ok(ExecutedBuilderNode { published, logger })
}

fn core_workspace(workspace: StoreWorkspace) -> Workspace {
    Workspace::new(
        workspace.log_dir().to_path_buf(),
        workspace.raw_log_dir().to_path_buf(),
        workspace.temp_dir().to_path_buf(),
    )
}

pub(crate) fn build_context(
    store: &Store,
    builder_run: &BuilderRun,
    build_key: BuildKey,
    logger: Arc<dyn BuildLogger>,
    cancellation: CancellationToken,
) -> Result<BuildContext, RuntimeError> {
    let cleanup = TempCleanupContext::new(store, builder_run.tag(), build_key);
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

pub(crate) fn map_identity_error(error: IdentityError) -> RuntimeError {
    RuntimeError::InvalidRequest(error.to_string())
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

    match recreate_store_temp_dir_force(&cleanup.store, temp_dir) {
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
    store: Store,
    builder_tag: String,
    build_key: BuildKey,
    mode: TempCleanupMode,
}

impl TempCleanupContext {
    fn new(store: &Store, builder_tag: &str, build_key: BuildKey) -> Self {
        Self {
            store: store.clone(),
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
                if let Err(error) = remove_store_temp_dir_force(&cleanup.store, temp_dir) {
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

    if let Err(error) = remove_store_temp_dir_force(&cleanup.store, temp_dir) {
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
        &cleanup.store,
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

enum TempCleanupPolicy {
    /// Builders may leave files owned by another namespace's uids, so their
    /// temp dirs are quarantined when they cannot be removed.
    Builder(TempCleanupContext),
    /// Sources materialize as the current user, so plain removal is enough.
    Source(Store),
}

/// RAII owner of a node's temp directory.
///
/// Created right after `create_workspace` (before the node logger exists), so
/// that every exit path — cache hit, error, success, or panic-unwind — cleans
/// the temp dir exactly once via `Drop`. The builder/source asymmetry is kept:
/// builders quarantine, sources remove. Until a node logger is attached with
/// [`TempDirGuard::set_logger`], cleanup warnings go to a no-op logger.
pub(crate) struct TempDirGuard {
    temp_dir: PathBuf,
    policy: TempCleanupPolicy,
    logger: Arc<dyn BuildLogger>,
}

impl TempDirGuard {
    pub(crate) fn for_builder(
        store: &Store,
        builder_tag: &str,
        build_key: BuildKey,
        temp_dir: PathBuf,
    ) -> Self {
        Self {
            temp_dir,
            policy: TempCleanupPolicy::Builder(TempCleanupContext::new(
                store,
                builder_tag,
                build_key,
            )),
            logger: Arc::new(NoopBuildLogger),
        }
    }

    pub(crate) fn for_source(store: &Store, temp_dir: PathBuf) -> Self {
        Self {
            temp_dir,
            policy: TempCleanupPolicy::Source(store.clone()),
            logger: Arc::new(NoopBuildLogger),
        }
    }

    pub(crate) fn set_logger(&mut self, logger: Arc<dyn BuildLogger>) {
        self.logger = logger;
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        match &self.policy {
            TempCleanupPolicy::Builder(cleanup) => {
                cleanup_temp_dir(&self.temp_dir, cleanup, self.logger.as_ref());
            }
            TempCleanupPolicy::Source(store) => {
                if let Err(error) = remove_store_temp_dir_force(store, &self.temp_dir) {
                    log_runtime_event(
                        self.logger.as_ref(),
                        BuildLogLevel::Warn,
                        "cleanup-warning",
                        format!(
                            "failed to remove temp dir '{}': {error}",
                            self.temp_dir.display()
                        ),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_store::{PublishRequest, create_workspace, publish_build};
    use mbuild_core::{
        BuildContext, BuildLogger, BuildRunLogger, BuilderInputs, BuilderRun, CancellationToken,
        InputSpec, StagedBuildResult, TypedBuilder, compute_build_key,
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

    fn create_test_logger(store: &Store) -> Arc<BuildRunLogger> {
        let locations = store.run_log_locations();
        Arc::new(BuildRunLogger::new(locations.run_log_dir(), locations.run_id(), false).unwrap())
    }

    fn create_test_builder_run(
        store: &Store,
        run_logger: &Arc<BuildRunLogger>,
        tag: &str,
        name: &str,
        build_key: BuildKey,
    ) -> (BuilderRun, Arc<dyn BuildLogger>) {
        let workspace = create_workspace(store, tag, Some(name.to_string()), build_key.to_string())
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

    fn metadata_log_dir(metadata: &Value) -> PathBuf {
        PathBuf::from(metadata["log_dir"].as_str().unwrap())
    }

    fn event_log_records(log_dir: &Path) -> Vec<Value> {
        fs::read_to_string(log_dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_quarantine_event(
        log_dir: &Path,
        level: &str,
        builder: &str,
        name: &str,
        build_key: BuildKey,
    ) -> Value {
        let events = event_log_records(log_dir)
            .into_iter()
            .filter(|event| event["phase"] == "temp-quarantine")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);

        let event = events.into_iter().next().unwrap();
        assert_eq!(event["level"], level);
        assert_eq!(event["builder"], builder);
        assert_eq!(event["name"], name);
        assert_eq!(event["details"]["full_build_key"], build_key.to_string());

        let message = event["message"].as_str().unwrap();
        assert!(message.contains("moved temp dir"));
        assert!(message.contains("to global quarantine"));
        event
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

    static RUNTIME_TEST_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    #[derive(Debug)]
    struct RuntimeTestBuilder;
    static RUNTIME_TEST_BUILDER: RuntimeTestBuilder = RuntimeTestBuilder;

    impl TypedBuilder for RuntimeTestBuilder {
        type Config = RuntimeTestConfig;

        fn tag(&self) -> &'static str {
            "RuntimeTest"
        }

        fn spec(&self) -> &'static InputSpec {
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

    static SANDBOX_RUNTIME_TEST_SPEC: InputSpec = InputSpec {
        required_inputs: &[],
        optional_inputs: &[],
        allow_extra_inputs: false,
    };

    #[derive(Debug)]
    struct SandboxRuntimeTestBuilder;
    static SANDBOX_RUNTIME_TEST_BUILDER: SandboxRuntimeTestBuilder = SandboxRuntimeTestBuilder;

    impl TypedBuilder for SandboxRuntimeTestBuilder {
        type Config = RuntimeTestConfig;

        fn tag(&self) -> &'static str {
            "Sandbox"
        }

        fn spec(&self) -> &'static InputSpec {
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

        fn tag(&self) -> &'static str {
            "RuntimeTest"
        }

        fn spec(&self) -> &'static InputSpec {
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

        fn tag(&self) -> &'static str {
            "RuntimeTest"
        }

        fn spec(&self) -> &'static InputSpec {
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
    fn resolve_reuse_for_build_depends_on_input_object_hash() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let matching_inputs = vec![
            "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
        ];
        let payload = json!({ "source": "echo hi\n", "executable": true });
        let reuse_key = compute_reuse_key("RuntimeLookupTest", &payload, &matching_inputs).unwrap();
        let build_key_for_object =
            BuildKey::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .unwrap();
        let lookup_build_key =
            BuildKey::from_str("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
                .unwrap();
        let stage = temp.path().join("script.sh");
        fs::write(&stage, b"echo hi\n").unwrap();
        publish_build(
            &store,
            PublishRequest {
                publication_name: "script".to_string(),
                build_key: build_key_for_object,
                reuse_key,
                staged_path: stage,
                inputs: matching_inputs.clone(),
            },
        )
        .unwrap();

        let matching_reuse_key =
            compute_reuse_key("RuntimeLookupTest", &payload, &matching_inputs).unwrap();
        let hit = resolve_reuse_for_build(&store, lookup_build_key, matching_reuse_key)
            .unwrap()
            .expect("expected canonical object hit");
        assert_eq!(hit.build.build_key, lookup_build_key);

        let mismatching_inputs = vec![
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .parse()
                .unwrap(),
        ];
        let mismatching_reuse_key =
            compute_reuse_key("RuntimeLookupTest", &payload, &mismatching_inputs).unwrap();
        assert!(
            resolve_reuse_for_build(&store, lookup_build_key, mismatching_reuse_key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn execute_builder_node_prepares_dirs_and_cleans_temp_on_success() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();

        let executed = execute_builder_node(ExecuteBuilderNodeRequest {
            store: &store,
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
        let store = create_test_store(temp.path());
        let config = json!({});
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let run_logger = create_test_logger(&store);
        let (builder_run, logger) = create_test_builder_run(
            &store,
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
            &store,
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
        assert_quarantine_event(
            builder_run.log_dir(),
            "warn",
            "RuntimeTest",
            "runtime-test",
            build_key,
        );
    }

    #[test]
    fn execute_sandbox_builder_quarantines_temp_without_removing_it() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();

        let executed = execute_builder_node(ExecuteBuilderNodeRequest {
            store: &store,
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
        let event = assert_quarantine_event(
            &metadata_log_dir(&metadata),
            "info",
            "Sandbox",
            "sandbox-runtime-test",
            build_key,
        );
        assert!(
            event["message"]
                .as_str()
                .unwrap()
                .contains("sandbox temp may contain userns-owned files")
        );
    }

    #[test]
    fn build_context_quarantines_stale_sandbox_temp_before_recreate() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let config = json!({});
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();
        let (builder_run, logger) = create_test_builder_run(
            &store,
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
            &store,
            &builder_run,
            build_key,
            logger,
            CancellationToken::new(),
        )
        .unwrap();

        assert!(context.temp_dir.is_dir());
        let event = assert_quarantine_event(
            builder_run.log_dir(),
            "info",
            "Sandbox",
            "sandbox-runtime-test",
            build_key,
        );
        assert!(
            event["message"]
                .as_str()
                .unwrap()
                .contains("stale sandbox temp dir may contain userns-owned files")
        );
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_failure() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let config = Value::Object(Map::new());
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            store: &store,
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
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let build_key = compute_build_key("RuntimeTest", &json!({}), &[]).unwrap();
        let (builder_run, logger) = create_test_builder_run(
            &store,
            &run_logger,
            "RuntimeTest",
            "runtime-test",
            build_key,
        );
        let temp_dir = builder_run.temp_dir().join("stale");
        fs::create_dir_all(temp_dir.parent().unwrap()).unwrap();
        fs::write(&temp_dir, b"not a directory\n").unwrap();

        let cleanup = TempCleanupContext::new(&store, "RuntimeTest", build_key);
        cleanup_temp_dir(&temp_dir, &cleanup, logger.as_ref());

        assert!(fs::symlink_metadata(&temp_dir).is_err());
        assert_quarantine_event(
            builder_run.log_dir(),
            "warn",
            "RuntimeTest",
            "runtime-test",
            build_key,
        );
    }

    #[test]
    fn temp_dir_guard_cleans_without_a_node_logger() {
        // Simulates the pre-logger window: a failure (e.g. bind_builder) before
        // a node logger is attached. The guard, created right after the
        // workspace, must still clean the temp dir on drop via the no-op logger.
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let build_key = compute_build_key("RuntimeTest", &json!({}), &[]).unwrap();
        let (builder_run, _logger) =
            create_test_builder_run(&store, &run_logger, "RuntimeTest", "guard-test", build_key);
        let temp_dir = builder_run.temp_dir().to_path_buf();
        assert!(temp_dir.is_dir());

        {
            // No set_logger call: the node logger was never bound.
            let _guard =
                TempDirGuard::for_builder(&store, "RuntimeTest", build_key, temp_dir.clone());
        }

        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_materialize_failure() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            store: &store,
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
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            store: &store,
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
}
