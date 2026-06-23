use bobr_store::{StoreError, StoreTempDir};
use mbuild_core::{
    BuildLogEvent, BuildLogLevel, BuildLogger, BuildStatus, BuilderError, CancellationToken,
    NoopBuildLogger,
};
use std::fmt;
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

/// Prepares an empty temp directory for a builder run. Builders own their
/// staging area, so this runs before
/// `BuilderPlannedSubject::execute` constructs its `BuildContext`.
pub(crate) fn prepare_temp(temp_dir: &StoreTempDir) -> Result<(), RuntimeError> {
    temp_dir.prepare_empty().map_err(map_store_error)
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
    status: BuildStatus,
    message: impl Into<String>,
) {
    logger.log_event(BuildLogEvent {
        level,
        status,
        op: None,
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

fn cleanup_temp_dir(temp_dir: &StoreTempDir, logger: &dyn BuildLogger) {
    if let Err(error) = temp_dir.remove_force() {
        log_runtime_event(
            logger,
            BuildLogLevel::Warn,
            BuildStatus::Cleanup,
            format!(
                "failed to remove temp dir '{}': {error}",
                temp_dir.path().display()
            ),
        );
    }
}

/// RAII owner of a node's temp directory.
///
/// Created right after `create_workspace` (before the node logger exists), so
/// that every post-workspace exit path — error, success, or panic-unwind —
/// cleans the temp dir exactly once via `Drop`. Cache hits return before a
/// workspace exists, so they do not need a temp guard. Until a node logger is
/// attached with [`TempDirGuard::set_logger`], cleanup warnings go to a no-op
/// logger.
pub(crate) struct TempDirGuard {
    temp_dir: StoreTempDir,
    logger: Arc<dyn BuildLogger>,
}

impl TempDirGuard {
    pub(crate) fn for_builder(temp_dir: StoreTempDir) -> Self {
        Self {
            temp_dir,
            logger: Arc::new(NoopBuildLogger),
        }
    }

    pub(crate) fn for_source(temp_dir: StoreTempDir) -> Self {
        Self {
            temp_dir,
            logger: Arc::new(NoopBuildLogger),
        }
    }

    pub(crate) fn set_logger(&mut self, logger: Arc<dyn BuildLogger>) {
        self.logger = logger;
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        cleanup_temp_dir(&self.temp_dir, self.logger.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planned::{
        PlannedExecutionContext, PlannedSubject, SubjectExecution, execute_subject,
    };
    use bobr_store::{
        Store, StoreWorkspace, create_workspace, materialize_build, resolve_reuse_for_build,
    };
    use mbuild_builder::{
        BuildContext, BuilderInputs, BuilderRegistry, InputSpec, StagedBuildResult, TypedBuilder,
    };
    use mbuild_core::{
        BuildKey, BuildLogSubject, BuildLogger, BuildRunLogger, CancellationToken, RuntimeProvider,
        compute_build_key, compute_reuse_key,
    };
    use serde::Deserialize;
    use serde_json::{Map, Value, json};
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::path::{Path, PathBuf};
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

    fn create_test_run(
        store: &Store,
        run_logger: &Arc<BuildRunLogger>,
        tag: &str,
        name: &str,
        build_key: BuildKey,
    ) -> (StoreWorkspace, Arc<dyn BuildLogger>) {
        let workspace =
            create_workspace(store, tag, Some(name.to_string()), build_key.to_string()).unwrap();
        let subject = BuildLogSubject::new(
            tag,
            name,
            build_key.to_string(),
            workspace.log_dir().to_path_buf(),
            workspace.raw_log_dir().to_path_buf(),
        );
        let logger = run_logger.bind_subject(subject).unwrap();
        (workspace, logger)
    }

    fn run_builder_subject(
        store: &Store,
        run_logger: Arc<BuildRunLogger>,
        builder: &'static dyn mbuild_builder::Builder,
        name: &str,
        config: Value,
        cancellation: CancellationToken,
    ) -> Result<SubjectExecution, RuntimeError> {
        let mut registry = BuilderRegistry::new();
        registry.register(builder).unwrap();
        let subject = registry
            .parse_subject(
                builder.tag(),
                json!({"name": name, "config": config})
                    .as_object()
                    .unwrap()
                    .clone(),
                BTreeMap::new(),
            )
            .unwrap();
        let realized_inputs = HashMap::new();
        execute_subject(
            &PlannedSubject::Builder(subject),
            PlannedExecutionContext {
                store,
                run_logger,
                runtime_provider: RuntimeProvider::host(),
                cancellation,
                realized_inputs: &realized_inputs,
            },
        )
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

    fn assert_cleanup_warning_event(
        log_dir: &Path,
        builder: &str,
        name: &str,
        build_key: BuildKey,
        message_fragment: &str,
    ) -> Value {
        let events = event_log_records(log_dir)
            .into_iter()
            .filter(|event| event["status"] == "cleanup")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);

        let event = events.into_iter().next().unwrap();
        assert_eq!(event["level"], "warn");
        assert_eq!(event["subject"]["tag"], builder);
        assert_eq!(event["subject"]["name"], name);
        assert_eq!(event["subject"]["build_key_full"], build_key.to_string());

        let message = event["message"].as_str().unwrap();
        assert!(message.contains(message_fragment));
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
            fs::write(cx.temp_dir.join("sandbox-scratch"), b"sandbox scratch\n").unwrap();

            Ok(StagedBuildResult {
                staged_path: cx.temp_dir.join("out"),
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
        materialize_build(
            &store,
            build_key_for_object,
            reuse_key,
            matching_inputs.clone(),
            &stage,
            Some("script"),
        )
        .unwrap();

        let matching_reuse_key =
            compute_reuse_key("RuntimeLookupTest", &payload, &matching_inputs).unwrap();
        let hit =
            resolve_reuse_for_build(&store, lookup_build_key, matching_reuse_key, Some("script"))
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
            resolve_reuse_for_build(
                &store,
                lookup_build_key,
                mismatching_reuse_key,
                Some("script")
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn execute_builder_node_prepares_dirs_and_cleans_temp_on_success() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let executed = run_builder_subject(
            &store,
            logger,
            &RUNTIME_TEST_BUILDER,
            "runtime-test",
            json!({}),
            CancellationToken::new(),
        )
        .unwrap();

        let metadata = workspace_metadata(temp.path(), "RuntimeTest", "runtime-test");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
        let object_path = store.object_path(executed.realized.object_hash);
        assert!(object_path.is_dir());
        assert!(object_path.join("payload").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_temp_reports_stale_temp_recreate_failure() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let config = json!({});
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let run_logger = create_test_logger(&store);
        let (workspace, _logger) = create_test_run(
            &store,
            &run_logger,
            "RuntimeTest",
            "runtime-test",
            build_key,
        );
        let temp_dir = workspace.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        let stale_target = temp.path().join("missing-stale-target");
        symlink(&stale_target, &temp_dir).unwrap();

        let error = prepare_temp(workspace.temp_dir_handle()).unwrap_err();

        assert_eq!(error.class(), "store");
        assert!(error.message().contains("failed to create directory"));
        assert!(
            fs::symlink_metadata(&temp_dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(event_log_records(workspace.log_dir()).is_empty());
    }

    #[test]
    fn execute_sandbox_builder_removes_temp_on_success() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);

        let executed = run_builder_subject(
            &store,
            logger,
            &SANDBOX_RUNTIME_TEST_BUILDER,
            "sandbox-runtime-test",
            json!({}),
            CancellationToken::new(),
        )
        .unwrap();

        let metadata = workspace_metadata(temp.path(), "Sandbox", "sandbox-runtime-test");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
        let object_path = store.object_path(executed.realized.object_hash);
        assert!(object_path.join("payload").is_file());
        assert!(
            event_log_records(&metadata_log_dir(&metadata))
                .iter()
                .all(|event| event["status"] != "cleanup")
        );
    }

    #[test]
    fn prepare_temp_removes_stale_sandbox_temp_before_recreate() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let config = json!({});
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();
        let (workspace, _logger) = create_test_run(
            &store,
            &run_logger,
            "Sandbox",
            "sandbox-runtime-test",
            build_key,
        );
        let temp_dir = workspace.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("stale"), b"old\n").unwrap();

        prepare_temp(workspace.temp_dir_handle()).unwrap();

        assert!(temp_dir.is_dir());
        assert_eq!(fs::read_dir(&temp_dir).unwrap().count(), 0);
        assert!(event_log_records(workspace.log_dir()).is_empty());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_failure() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let error = run_builder_subject(
            &store,
            logger,
            &RUNTIME_FAIL_BUILDER,
            "runtime-fail",
            Value::Object(Map::new()),
            CancellationToken::new(),
        )
        .unwrap_err();

        assert_eq!(error.class(), "build");
        let metadata = workspace_metadata(temp.path(), "RuntimeTest", "runtime-fail");
        let temp_dir = metadata_temp_dir(&metadata);
        assert!(!temp_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_temp_dir_warns_when_remove_fails() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let build_key = compute_build_key("RuntimeTest", &json!({}), &[]).unwrap();
        let (workspace, logger) = create_test_run(
            &store,
            &run_logger,
            "RuntimeTest",
            "runtime-test",
            build_key,
        );
        let temp_dir = workspace.temp_dir().to_path_buf();
        fs::remove_dir_all(&temp_dir).unwrap();
        fs::write(&temp_dir, b"not a directory\n").unwrap();

        cleanup_temp_dir(workspace.temp_dir_handle(), logger.as_ref());

        assert!(temp_dir.is_file());
        assert_cleanup_warning_event(
            workspace.log_dir(),
            "RuntimeTest",
            "runtime-test",
            build_key,
            "failed to remove temp dir",
        );
    }

    #[test]
    fn temp_dir_guard_cleans_without_a_node_logger() {
        // Simulates the pre-logger window: a failure (e.g. bind_subject) before
        // a node logger is attached. The guard, created right after the
        // workspace, must still clean the temp dir on drop via the no-op logger.
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let run_logger = create_test_logger(&store);
        let build_key = compute_build_key("RuntimeTest", &json!({}), &[]).unwrap();
        let (workspace, _logger) =
            create_test_run(&store, &run_logger, "RuntimeTest", "guard-test", build_key);
        let temp_dir = workspace.temp_dir().to_path_buf();
        assert!(temp_dir.is_dir());

        {
            // No set_logger call: the node logger was never bound.
            let _guard = TempDirGuard::for_builder(workspace.temp_dir_handle().clone());
        }

        assert!(!temp_dir.exists());
    }

    #[test]
    fn execute_builder_node_cleans_temp_on_materialize_failure() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let logger = create_test_logger(&store);
        let error = run_builder_subject(
            &store,
            logger,
            &RUNTIME_BROKEN_STAGE_BUILDER,
            "runtime-broken-stage",
            json!({}),
            CancellationToken::new(),
        )
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
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = run_builder_subject(
            &store,
            logger,
            &RUNTIME_TEST_BUILDER,
            "runtime-test",
            json!({}),
            cancellation,
        )
        .unwrap_err();

        assert_eq!(error.class(), "cancelled");
        assert_eq!(workspace_count(temp.path()), 0);
    }
}
