use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use crate::runtime::{
    RuntimeError, TempDirGuard, check_cancelled, log_runtime_event, map_builder_error,
    map_store_error, prepare_temp,
};
use bobr_store::{
    ObjectRecord, RealizedObject, SourceImportOutcome, Store, create_workspace,
    import_source_object, materialize_build, record_existing_source_object, resolve_build_handle,
    resolve_reuse_for_build,
};
use mbuild_builder::{BuilderPlanError, BuilderPlannedSubject};
use mbuild_core::{
    BuildKey, BuildLogLevel, BuildLogger, BuildRunLogger, BuildStatus, CancellationToken,
    NoopBuildLogger, SubjectRunContext, Workspace,
};
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
    pub(crate) runtime_provider: mbuild_core::RuntimeProvider,
    pub(crate) cancellation: CancellationToken,
    pub(crate) realized_inputs: &'a HashMap<BuildKey, RealizedInput>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubjectExecution {
    pub(crate) realized: RealizedObject,
    pub(crate) logger: Arc<dyn BuildLogger>,
}

#[derive(Debug, Clone)]
pub(crate) struct RealizedInput {
    pub(crate) realized: RealizedObject,
    pub(crate) materialization_name: String,
}

pub(crate) fn execute_subject(
    subject: &PlannedSubject,
    cx: PlannedExecutionContext<'_>,
) -> Result<SubjectExecution, RuntimeError> {
    match subject {
        PlannedSubject::Source(subject) => execute_source_subject(subject, cx),
        PlannedSubject::Builder(subject) => execute_builder_subject(subject, cx),
    }
}

fn execute_builder_subject(
    subject: &BuilderPlannedSubject,
    cx: PlannedExecutionContext<'_>,
) -> Result<SubjectExecution, RuntimeError> {
    let build_key = subject.build_key();
    let inputs = builder_resolved_inputs(subject, cx.store, cx.realized_inputs)?;
    check_cancelled(&cx.cancellation)?;
    let input_hashes = inputs
        .ordered_reuse_input_hashes(subject.input_spec())
        .map_err(map_builder_error)?;

    // Resolve the caches before building a workspace: a hit needs no
    // workspace, logger, or temp dir, and is left silent (NoopBuildLogger).
    if let Some(published) =
        resolve_build_handle(cx.store, build_key, Some(subject.name())).map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &published.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }
    let reuse_key = subject
        .compute_reuse_key(&input_hashes)
        .map_err(map_builder_plan_error)?;
    if let Some(published) =
        resolve_reuse_for_build(cx.store, build_key, reuse_key, Some(subject.name()))
            .map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &published.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }
    let builder_inputs =
        inputs.prepare_builder_inputs(subject.input_spec(), cx.store, &cx.runtime_provider)?;

    // Miss: create the workspace and run the builder.
    let store_workspace = create_workspace(
        cx.store,
        subject.tag(),
        Some(subject.name().to_string()),
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
        .map_err(RuntimeError::Store)?;
    temp_guard.set_logger(logger.clone());
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Start,
        "starting subject",
    );
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::CacheMiss,
        "executing builder",
    );
    check_cancelled(&cx.cancellation)?;
    prepare_temp(&temp_dir_handle)?;
    log_runtime_event(
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
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                BuildStatus::Failed,
                error.to_string(),
            );
            map_builder_error(error)
        })?;
    check_cancelled(&cx.cancellation)?;
    let published = materialize_build(
        cx.store,
        build_key,
        reuse_key,
        input_hashes,
        &staged.staged_path,
        Some(subject.name()),
    )
    .map_err(|error| {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Error,
            BuildStatus::Failed,
            error.to_string(),
        );
        map_store_error(error)
    })?;
    Ok(SubjectExecution {
        realized: realized_object_from_record(Some(build_key), &published.object_record),
        logger,
    })
}

fn execute_source_subject(
    subject: &SourcePlannedSubject,
    cx: PlannedExecutionContext<'_>,
) -> Result<SubjectExecution, RuntimeError> {
    let build_key = subject.build_key();
    check_cancelled(&cx.cancellation)?;

    // Resolve the caches before building a workspace: a hit needs no
    // workspace, logger, or temp dir, and is left silent (NoopBuildLogger).
    if let Some(published) =
        resolve_build_handle(cx.store, build_key, Some(subject.name())).map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &published.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }
    if let Some(stored) = record_existing_source_object(
        cx.store,
        subject.declared_object_hash(),
        Some(subject.name()),
    )
    .map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &stored.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }

    // Miss: create the workspace and materialize the source.
    let store_workspace = create_workspace(
        cx.store,
        "Source",
        Some(subject.name().to_string()),
        build_key.to_string(),
    )
    .map_err(map_store_error)?;
    let temp_dir_handle = store_workspace.temp_dir_handle().clone();
    let workspace = core_workspace(store_workspace);
    // Owns the temp dir from here on: every return path (bind error below, and
    // panics) cleans it via Drop.
    let mut temp_guard = TempDirGuard::for_source(temp_dir_handle);
    let logger = cx
        .run_logger
        .bind_subject(subject.log_subject(&workspace))
        .map_err(RuntimeError::Store)?;
    temp_guard.set_logger(logger.clone());
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Start,
        "starting subject",
    );
    log_runtime_event(
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
        log_runtime_event(
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
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        BuildStatus::Running,
        "materializing source origin",
    );

    let import_outcome = import_source_object(
        cx.store,
        subject.declared_object_hash(),
        &staged_path,
        Some(subject.name()),
    )
    .map_err(map_store_error)?;
    check_cancelled(&cx.cancellation)?;

    match import_outcome {
        SourceImportOutcome::Matched(stored) => Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &stored.object_record),
            logger,
        }),
        SourceImportOutcome::Mismatched { actual_hash } => {
            let message = format!(
                "source '{}' materialized unexpected object hash: expected {}, got {}",
                subject.name(),
                subject.declared_object_hash(),
                actual_hash
            );
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                BuildStatus::Failed,
                &message,
            );
            Err(RuntimeError::Build(message))
        }
    }
}

fn builder_resolved_inputs(
    subject: &BuilderPlannedSubject,
    store: &Store,
    realized_inputs: &HashMap<BuildKey, RealizedInput>,
) -> Result<ResolvedInputs, RuntimeError> {
    let mut inputs = ResolvedInputs::empty();
    for input_name in subject
        .input_spec()
        .ordered_present_input_names(subject.inputs())
    {
        let key = *subject.inputs().get(input_name).ok_or_else(|| {
            RuntimeError::Store(format!(
                "planned builder input '{}' is missing for '{}'",
                input_name,
                subject.name()
            ))
        })?;
        let input = realized_inputs.get(&key).cloned().ok_or_else(|| {
            RuntimeError::Build(format!(
                "dependency object '{}' is not available in completed set",
                key
            ))
        })?;
        inputs.insert(
            input_name,
            ResolvedDependency {
                object_hash: input.realized.object_hash,
                object_path: store.object_path(input.realized.object_hash),
                materialization_name: Some(input.materialization_name),
            },
        );
    }
    Ok(inputs)
}

fn map_builder_plan_error(error: BuilderPlanError) -> RuntimeError {
    match error {
        BuilderPlanError::UnknownBuilder { .. } => RuntimeError::UnknownBuilder(error.to_string()),
        BuilderPlanError::Recipe(_) => RuntimeError::RecipeLoad(error.to_string()),
        BuilderPlanError::InvalidRequest(_) | BuilderPlanError::Identity(_) => {
            RuntimeError::InvalidRequest(error.to_string())
        }
    }
}

fn map_source_execution_error(error: SourceExecutionError) -> RuntimeError {
    match error {
        SourceExecutionError::Cancelled(message) => RuntimeError::Cancelled(message),
        SourceExecutionError::Build(message) => RuntimeError::Build(message),
    }
}

pub(crate) fn realized_object_from_record(
    build_key: Option<BuildKey>,
    object_record: &ObjectRecord,
) -> RealizedObject {
    RealizedObject {
        build_key,
        object_hash: object_record.object_hash,
        run_id: object_record.run_id.clone(),
    }
}

pub(crate) fn core_workspace(workspace: bobr_store::StoreWorkspace) -> Workspace {
    Workspace::new(
        workspace.log_dir().to_path_buf(),
        workspace.raw_log_dir().to_path_buf(),
        workspace.temp_dir().to_path_buf(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::ObjectHash;
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
