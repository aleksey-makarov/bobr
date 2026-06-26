use crate::execution::{
    ExecutionError, TempDirGuard, check_cancelled, log_execution_event, map_builder_error,
    map_store_error, prepare_temp,
};
use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use bobr_core::{
    BuildKey, BuildLogLevel, BuildLogger, BuildRunLogger, BuildStatus, CancellationToken,
    NoopBuildLogger, ObjectHash, SubjectRunContext, Workspace,
};
use bobr_store::{
    SourceImportOutcome, Store, create_workspace, import_build, import_source_object,
    record_existing_source_object, resolve_build_handle, resolve_reuse_for_build,
};
use mbuild_builder::{BuilderPlanError, BuilderPlannedSubject};
use mbuild_source::{SourceExecutionError, SourcePlannedSubject};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) enum PlannedSubject {
    Source(SourcePlannedSubject),
    Builder(BuilderPlannedSubject),
}

impl PlannedSubject {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Source(subject) => subject.name(),
            Self::Builder(subject) => subject.name(),
        }
    }

    pub(crate) fn as_builder(&self) -> Option<&BuilderPlannedSubject> {
        match self {
            Self::Source(_) => None,
            Self::Builder(subject) => Some(subject),
        }
    }
}

pub(crate) struct PlannedExecutionContext<'a> {
    pub(crate) store: &'a Store,
    pub(crate) run_logger: Arc<BuildRunLogger>,
    pub(crate) runtime_provider: bobr_core::RuntimeProvider,
    pub(crate) cancellation: CancellationToken,
    pub(crate) realized_inputs: &'a HashMap<BuildKey, RealizedInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubjectOutcome {
    /// Resolved from cache without a workspace or per-subject logger.
    CacheHit,
    /// Built (or materialized) in this run through a bound subject logger.
    Built,
}

#[derive(Debug, Clone)]
pub(crate) struct SubjectExecution {
    pub(crate) object_hash: ObjectHash,
    pub(crate) logger: Arc<dyn BuildLogger>,
    pub(crate) outcome: SubjectOutcome,
}

#[derive(Debug, Clone)]
pub(crate) struct RealizedInput {
    pub(crate) object_hash: ObjectHash,
    pub(crate) materialization_name: String,
}

pub(crate) fn execute_subject(
    subject: &PlannedSubject,
    cx: PlannedExecutionContext<'_>,
) -> Result<SubjectExecution, ExecutionError> {
    match subject {
        PlannedSubject::Source(subject) => execute_source_subject(subject, cx),
        PlannedSubject::Builder(subject) => execute_builder_subject(subject, cx),
    }
}

fn execute_builder_subject(
    subject: &BuilderPlannedSubject,
    cx: PlannedExecutionContext<'_>,
) -> Result<SubjectExecution, ExecutionError> {
    let build_key = subject.build_key();
    let inputs = builder_resolved_inputs(subject, cx.store, cx.realized_inputs)?;
    check_cancelled(&cx.cancellation)?;
    let input_hashes = inputs.reuse_input_hashes();

    // Resolve the caches before building a workspace: a hit needs no
    // workspace, logger, or temp dir, and is left silent (NoopBuildLogger).
    if let Some(object_hash) =
        resolve_build_handle(cx.store, build_key, subject.name()).map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            object_hash,
            logger: Arc::new(NoopBuildLogger),
            outcome: SubjectOutcome::CacheHit,
        });
    }
    let reuse_key = subject
        .compute_reuse_key(&input_hashes)
        .map_err(map_builder_plan_error)?;
    if let Some(object_hash) =
        resolve_reuse_for_build(cx.store, build_key, reuse_key, subject.name())
            .map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            object_hash,
            logger: Arc::new(NoopBuildLogger),
            outcome: SubjectOutcome::CacheHit,
        });
    }
    let builder_inputs =
        inputs.prepare_builder_inputs(subject.input_spec(), cx.store, &cx.runtime_provider)?;

    // Miss: create the workspace and run the builder.
    let store_workspace = create_workspace(
        cx.store,
        subject.tag(),
        subject.name(),
        build_key.to_string(),
    )
    .map_err(map_store_error)?;
    let temp_dir_handle = store_workspace.temp_dir_handle().clone();
    let workspace = core_workspace(store_workspace);
    // Owns the temp dir from here on: every return path (bind error below, and
    // panics) cleans it via Drop.
    let mut temp_guard = TempDirGuard::for_builder(temp_dir_handle.clone());
    let logger = cx
        .run_logger
        .bind_subject(subject.log_subject(&workspace))
        .map_err(ExecutionError::Store)?;
    temp_guard.set_logger(logger.clone());
    log_execution_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Start,
        "starting subject",
    );
    log_execution_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::CacheMiss,
        "executing builder",
    );
    check_cancelled(&cx.cancellation)?;
    prepare_temp(&temp_dir_handle)?;
    log_execution_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Running,
        "running builder implementation",
    );
    let ctx = SubjectRunContext::new(
        workspace,
        logger.clone(),
        cx.cancellation.clone(),
        cx.runtime_provider.clone(),
    );
    let staged = subject
        .execute(&ctx, builder_inputs, Some(cx.store.fs_tree()))
        .map_err(|error| {
            log_execution_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                BuildStatus::Failed,
                error.to_string(),
            );
            map_builder_error(error)
        })?;
    check_cancelled(&cx.cancellation)?;
    let object_hash = import_build(
        cx.store,
        build_key,
        reuse_key,
        input_hashes.values().copied().collect(),
        &staged.staged_path,
        subject.name(),
    )
    .map_err(|error| {
        log_execution_event(
            logger.as_ref(),
            BuildLogLevel::Error,
            BuildStatus::Failed,
            error.to_string(),
        );
        map_store_error(error)
    })?;
    Ok(SubjectExecution {
        object_hash,
        logger,
        outcome: SubjectOutcome::Built,
    })
}

fn execute_source_subject(
    subject: &SourcePlannedSubject,
    cx: PlannedExecutionContext<'_>,
) -> Result<SubjectExecution, ExecutionError> {
    let build_key = subject.build_key();
    check_cancelled(&cx.cancellation)?;

    // Resolve the caches before building a workspace: a hit needs no
    // workspace, logger, or temp dir, and is left silent (NoopBuildLogger).
    if let Some(object_hash) =
        resolve_build_handle(cx.store, build_key, subject.name()).map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            object_hash,
            logger: Arc::new(NoopBuildLogger),
            outcome: SubjectOutcome::CacheHit,
        });
    }
    if let Some(object_hash) =
        record_existing_source_object(cx.store, subject.declared_object_hash(), subject.name())
            .map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            object_hash,
            logger: Arc::new(NoopBuildLogger),
            outcome: SubjectOutcome::CacheHit,
        });
    }

    // Miss: create the workspace and materialize the source.
    let store_workspace =
        create_workspace(cx.store, "Source", subject.name(), build_key.to_string())
            .map_err(map_store_error)?;
    let temp_dir_handle = store_workspace.temp_dir_handle().clone();
    let workspace = core_workspace(store_workspace);
    // Owns the temp dir from here on: every return path (bind error below, and
    // panics) cleans it via Drop.
    let mut temp_guard = TempDirGuard::for_source(temp_dir_handle);
    let logger = cx
        .run_logger
        .bind_subject(subject.log_subject(&workspace))
        .map_err(ExecutionError::Store)?;
    temp_guard.set_logger(logger.clone());
    log_execution_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Start,
        "starting subject",
    );
    log_execution_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::CacheMiss,
        "materializing source",
    );

    check_cancelled(&cx.cancellation)?;
    let ctx = SubjectRunContext::new(
        workspace,
        logger.clone(),
        cx.cancellation.clone(),
        cx.runtime_provider.clone(),
    );
    let staged_path = subject.execute(&ctx).map_err(|error| {
        log_execution_event(
            logger.as_ref(),
            BuildLogLevel::Error,
            BuildStatus::Failed,
            error.to_string(),
        );
        map_source_execution_error(error)
    })?;
    // `origin.materialize` has no cancellation hook, so honor a cancel that
    // arrived while staging before publishing the imported object.
    check_cancelled(&cx.cancellation)?;
    log_execution_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Running,
        "materializing source origin",
    );

    let import_outcome = import_source_object(
        cx.store,
        subject.declared_object_hash(),
        &staged_path,
        subject.name(),
    )
    .map_err(map_store_error)?;
    check_cancelled(&cx.cancellation)?;

    match import_outcome {
        SourceImportOutcome::Matched(object_hash) => Ok(SubjectExecution {
            object_hash,
            logger,
            outcome: SubjectOutcome::Built,
        }),
        SourceImportOutcome::Mismatched { actual_hash } => {
            let message = format!(
                "source '{}' materialized unexpected object hash: expected {}, got {}",
                subject.name(),
                subject.declared_object_hash(),
                actual_hash
            );
            log_execution_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                BuildStatus::Failed,
                &message,
            );
            Err(ExecutionError::Build(message))
        }
    }
}

fn builder_resolved_inputs(
    subject: &BuilderPlannedSubject,
    store: &Store,
    realized_inputs: &HashMap<BuildKey, RealizedInput>,
) -> Result<ResolvedInputs, ExecutionError> {
    let mut inputs = ResolvedInputs::empty();
    for input_name in subject
        .input_spec()
        .ordered_present_input_names(subject.inputs())
    {
        let key = *subject.inputs().get(input_name).ok_or_else(|| {
            ExecutionError::Store(format!(
                "planned builder input '{}' is missing for '{}'",
                input_name,
                subject.name()
            ))
        })?;
        let input = realized_inputs.get(&key).cloned().ok_or_else(|| {
            ExecutionError::Build(format!(
                "dependency object '{}' is not available in completed set",
                key
            ))
        })?;
        inputs.insert(
            input_name,
            ResolvedDependency {
                object_hash: input.object_hash,
                object_path: store
                    .object_path(input.object_hash)
                    .map_err(map_store_error)?
                    .ok_or_else(|| {
                        ExecutionError::Build(format!(
                            "input object '{}' is missing from store",
                            input.object_hash
                        ))
                    })?,
                materialization_name: Some(input.materialization_name),
            },
        );
    }
    Ok(inputs)
}

fn map_builder_plan_error(error: BuilderPlanError) -> ExecutionError {
    match error {
        BuilderPlanError::UnknownBuilder { .. } => {
            ExecutionError::UnknownBuilder(error.to_string())
        }
        BuilderPlanError::Recipe(_) => ExecutionError::RequestLoad(error.to_string()),
        BuilderPlanError::InvalidRequest(_) | BuilderPlanError::Identity(_) => {
            ExecutionError::InvalidRequest(error.to_string())
        }
    }
}

fn map_source_execution_error(error: SourceExecutionError) -> ExecutionError {
    match error {
        SourceExecutionError::Cancelled(message) => ExecutionError::Cancelled(message),
        SourceExecutionError::Build(message) => ExecutionError::Build(message),
    }
}

fn core_workspace(workspace: bobr_store::StoreWorkspace) -> Workspace {
    Workspace::new(
        workspace.log_dir().to_path_buf(),
        workspace.raw_log_dir().to_path_buf(),
        workspace.temp_dir().to_path_buf(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::ObjectHash;
    use std::str::FromStr;

    #[test]
    fn source_subject_exposes_build_key_and_declared_hash() {
        let object_hash = ObjectHash::from_str(
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let subject = SourcePlannedSubject::new("source".to_string(), object_hash, None);

        assert_eq!(subject.name(), "source");
        assert_eq!(subject.tag(), "Source");
        assert_eq!(subject.build_key(), BuildKey::from_object_hash(object_hash));
        assert_eq!(subject.declared_object_hash(), object_hash);
    }
}
