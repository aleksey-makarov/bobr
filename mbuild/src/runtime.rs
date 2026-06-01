use crate::logging::BuildRunLogger;
use crate::resolved_inputs::ResolvedInputs;
use mbuild_core::{
    Build, BuildContext, BuildKey, BuildLogEvent, BuildLogLevel, BuildLogger, Builder,
    BuilderError, CancellationToken, CasError, PublishedBuild, ResultInputIdentity, StoreLayout,
    compute_reuse_key, fsutil, load_build_handle, load_reuse_record, materialize_build,
    object_path,
};
use serde_json::{Value, json};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

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
    pub(crate) layout: &'a StoreLayout,
    pub(crate) builder: &'static dyn Builder,
    pub(crate) build_key: BuildKey,
    pub(crate) build_name: &'a str,
    pub(crate) created_at: &'a str,
    pub(crate) run_logger: Arc<BuildRunLogger>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) config: Value,
    pub(crate) inputs: ResolvedInputs,
}

pub(crate) fn execute_builder_node(
    request: ExecuteBuilderNodeRequest<'_>,
) -> Result<PublishedBuild, RuntimeError> {
    let ExecuteBuilderNodeRequest {
        layout,
        builder,
        build_key,
        build_name,
        created_at,
        run_logger,
        cancellation,
        config,
        inputs,
    } = request;

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
                created_at: result.created_at.clone(),
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
    let cleanup = TempCleanupContext::new(layout, builder.spec().tag, build_key);
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
            cleanup_temp_dir(&context.temp_dir, &cleanup, logger.as_ref());
            runtime_error
        })?;
    if let Err(error) = check_cancelled(&cancellation) {
        cleanup_temp_dir(&context.temp_dir, &cleanup, logger.as_ref());
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
    cleanup_temp_dir(&context.temp_dir, &cleanup, logger.as_ref());
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
            created_at: result.created_at.clone(),
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
    let cleanup = TempCleanupContext::new(layout, builder_tag, build_key);
    recreate_empty_temp_dir_with_quarantine(&temp_dir, &cleanup, logger.as_ref())?;
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

fn recreate_empty_temp_dir_with_quarantine(
    temp_dir: &Path,
    cleanup: &TempCleanupContext,
    logger: &dyn BuildLogger,
) -> Result<(), RuntimeError> {
    if cleanup.mode == TempCleanupMode::DirectQuarantine {
        if fs::symlink_metadata(temp_dir).is_ok() {
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

    match fsutil::recreate_empty_dir_force(temp_dir) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempCleanupMode {
    RemoveThenQuarantine,
    DirectQuarantine,
}

#[derive(Debug)]
struct TempCleanupContext {
    quarantine_dir: PathBuf,
    builder_tag: String,
    build_key: BuildKey,
    mode: TempCleanupMode,
}

impl TempCleanupContext {
    fn new(layout: &StoreLayout, builder_tag: &str, build_key: BuildKey) -> Self {
        Self {
            quarantine_dir: layout.root.join("quarantine"),
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

    if let Err(error) = fsutil::remove_dir_force(temp_dir) {
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
    let name = temp_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid temp dir path '{}'", temp_dir.display()))?;
    let quarantine_dir = &cleanup.quarantine_dir;
    fs::create_dir_all(quarantine_dir).map_err(|error| {
        format!(
            "failed to create quarantine directory '{}': {error}",
            quarantine_dir.display()
        )
    })?;
    let stamp = fsutil::current_epoch_nanos().map_err(|error| error.to_string())?;
    let timestamp = human_quarantine_timestamp(stamp)?;

    for counter in 1..1000 {
        let suffix = if counter == 1 {
            timestamp.clone()
        } else {
            format!("{timestamp}.{counter}")
        };
        let target = quarantine_dir.join(format!(
            "{}-{}-{}-{name}",
            suffix,
            safe_quarantine_component(&cleanup.builder_tag),
            cleanup.build_key.to_hex(),
        ));
        if target.exists() || target.is_symlink() {
            continue;
        }
        match fs::rename(temp_dir, &target) {
            Ok(()) => {
                write_quarantine_metadata(&target, temp_dir, cleanup, &reason, stamp, logger);
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
                return Ok(target);
            }
            Err(_) if target.exists() || target.is_symlink() => continue,
            Err(error) => {
                return Err(format!(
                    "failed to move temp dir '{}' to '{}': {error}",
                    temp_dir.display(),
                    target.display()
                ));
            }
        }
    }

    Err(format!(
        "failed to find quarantine target for temp dir '{}' under '{}'",
        temp_dir.display(),
        quarantine_dir.display()
    ))
}

fn human_quarantine_timestamp(stamp: u128) -> Result<String, String> {
    let nanos = i128::try_from(stamp)
        .map_err(|_| format!("quarantine timestamp is out of range: {stamp}"))?;
    let parsed = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|error| format!("failed to parse quarantine timestamp {stamp}: {error}"))?;
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local = parsed.to_offset(offset);
    let format = format_description!("[year repr:last_two][month][day][hour][minute][second]");
    local
        .format(&format)
        .map_err(|error| format!("failed to format quarantine timestamp: {error}"))
}

fn safe_quarantine_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn write_quarantine_metadata(
    target: &Path,
    original_path: &Path,
    cleanup: &TempCleanupContext,
    reason: &str,
    stamp: u128,
    logger: &dyn BuildLogger,
) {
    let Some(file_name) = target.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let metadata_path = target.with_file_name(format!("{file_name}.json"));
    let metadata = json!({
        "schema": "mbuild-quarantine-v1",
        "builder_tag": &cleanup.builder_tag,
        "build_key": cleanup.build_key.to_hex(),
        "original_path": original_path.display().to_string(),
        "quarantine_path": target.display().to_string(),
        "reason": reason,
        "created_at_unix_nanos": stamp.to_string(),
    });
    match serde_json::to_vec_pretty(&metadata) {
        Ok(bytes) => {
            if let Err(error) = fs::write(&metadata_path, bytes) {
                log_runtime_event(
                    logger,
                    BuildLogLevel::Warn,
                    "cleanup-warning",
                    format!(
                        "failed to write quarantine metadata '{}': {error}",
                        metadata_path.display()
                    ),
                );
            }
        }
        Err(error) => {
            log_runtime_event(
                logger,
                BuildLogLevel::Warn,
                "cleanup-warning",
                format!("failed to encode quarantine metadata: {error}"),
            );
        }
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
            assert!(cx.state_dir.is_dir());
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
                staged_path: cx.temp_dir.join("missing-output"),
                object_hash: None,
            })
        }
    }

    #[test]
    fn lookup_canonical_result_depends_on_input_object_hash() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let matching_inputs = vec![ResultInputIdentity {
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

        let mismatching_inputs = vec![ResultInputIdentity {
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
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let logger = Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let state_dir = layout.root.join("builder-state").join("runtimetest");
        let temp_dir = state_dir.join("tmp").join(build_key.to_hex());
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("stale"), b"old\n").unwrap();

        let published = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_TEST_BUILDER,
            build_key,
            build_name: "runtime-test",
            created_at: "2026-04-05T12:00:00.000000000Z",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap();

        assert!(state_dir.is_dir());
        assert!(!temp_dir.exists());
        assert!(published.object_path.is_dir());
        assert!(published.object_path.join("payload").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn build_context_quarantines_stale_temp_dir_when_recreate_fails() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let config = json!({});
        let build_key = compute_build_key("RuntimeTest", &config, &[]).unwrap();
        let run_logger =
            Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let logger = run_logger.bind_node("RuntimeTest", "runtime-test", build_key);
        let state_dir = layout.root.join("builder-state").join("runtimetest");
        let temp_dir = state_dir.join("tmp").join(build_key.to_hex());
        fs::create_dir_all(temp_dir.parent().unwrap()).unwrap();
        let stale_target = temp.path().join("missing-stale-target");
        symlink(&stale_target, &temp_dir).unwrap();

        let context = build_context(
            &layout,
            "RuntimeTest",
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
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let logger = Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();
        let state_dir = layout.root.join("builder-state").join("sandbox");
        let temp_dir = state_dir.join("tmp").join(build_key.to_hex());

        let published = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &SANDBOX_RUNTIME_TEST_BUILDER,
            build_key,
            build_name: "sandbox-runtime-test",
            created_at: "2026-04-05T12:00:00.000000000Z",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap();

        assert!(!temp_dir.exists());
        assert!(published.object_path.join("payload").is_file());
        let quarantined = quarantine_entries(&layout);
        assert_eq!(quarantined.len(), 1);
        assert!(quarantined[0].join("sandbox-scratch").is_file());
        assert_quarantine_metadata(&quarantined[0], "Sandbox", build_key);
    }

    #[test]
    fn execute_sandbox_builder_quarantines_stale_temp_before_recreate() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let logger = Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let config = json!({});
        let inputs = ResolvedInputs::empty();
        let build_key = compute_build_key("Sandbox", &config, &[]).unwrap();
        let state_dir = layout.root.join("builder-state").join("sandbox");
        let temp_dir = state_dir.join("tmp").join(build_key.to_hex());
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("stale"), b"old\n").unwrap();

        execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &SANDBOX_RUNTIME_TEST_BUILDER,
            build_key,
            build_name: "sandbox-runtime-test",
            created_at: "2026-04-05T12:00:00.000000000Z",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap();

        assert!(!temp_dir.exists());
        let quarantined = quarantine_entries(&layout);
        assert_eq!(quarantined.len(), 2);
        assert!(quarantined.iter().any(|path| path.join("stale").is_file()));
        assert!(
            quarantined
                .iter()
                .any(|path| path.join("sandbox-scratch").is_file())
        );
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

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_FAIL_BUILDER,
            build_key,
            build_name: "runtime-fail",
            created_at: "2026-04-05T12:00:00.000000000Z",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
        .unwrap_err();

        assert_eq!(error.class(), "build");
        assert!(!temp_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_temp_dir_quarantines_when_remove_fails() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let run_logger =
            Arc::new(BuildRunLogger::new(&layout.root, RunOptions::default()).unwrap());
        let logger = run_logger.bind_node(
            "RuntimeTest",
            "runtime-test",
            compute_build_key("RuntimeTest", &json!({}), &[]).unwrap(),
        );
        let state_dir = layout.root.join("builder-state").join("runtimetest");
        let temp_dir = state_dir.join("tmp").join("stale");
        fs::create_dir_all(temp_dir.parent().unwrap()).unwrap();
        fs::write(&temp_dir, b"not a directory\n").unwrap();

        let cleanup = TempCleanupContext::new(
            &layout,
            "RuntimeTest",
            compute_build_key("RuntimeTest", &json!({}), &[]).unwrap(),
        );
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

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_BROKEN_STAGE_BUILDER,
            build_key,
            build_name: "runtime-broken-stage",
            created_at: "2026-04-05T12:00:00.000000000Z",
            run_logger: logger,
            cancellation: CancellationToken::new(),
            config,
            inputs,
        })
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

        let error = execute_builder_node(ExecuteBuilderNodeRequest {
            layout: &layout,
            builder: &RUNTIME_TEST_BUILDER,
            build_key,
            build_name: "runtime-test",
            created_at: "2026-04-05T12:00:00.000000000Z",
            run_logger: logger,
            cancellation,
            config,
            inputs,
        })
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

    fn quarantine_entries(layout: &StoreLayout) -> Vec<PathBuf> {
        let mut entries = fs::read_dir(layout.root.join("quarantine"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) != Some("json"))
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    fn assert_quarantine_metadata(path: &Path, builder_tag: &str, build_key: BuildKey) {
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let metadata_path = path.with_file_name(format!("{file_name}.json"));
        let metadata: Value = serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        assert_eq!(metadata["schema"], "mbuild-quarantine-v1");
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
        assert_eq!(
            name_timestamp,
            human_quarantine_timestamp(created_at).unwrap()
        );
    }
}
