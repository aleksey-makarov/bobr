use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use crate::runtime::{
    RuntimeError, TempDirGuard, build_context, check_cancelled, log_runtime_event,
    map_builder_error, map_identity_error, map_store_error,
};
use bobr_store::{
    ObjectRecord, RealizedObject, SourceImportOutcome, Store, create_workspace,
    import_source_object, load_build_handle, materialize_build,
    materialize_build_with_trusted_hash, record_existing_source_object, resolve_reuse_for_build,
};
use mbuild_core::{
    BuildKey, BuildLogLevel, BuildLogger, BuildRunLogger, Builder, BuilderClassBase,
    BuilderRunInit, CancellationToken, NoopBuildLogger, OriginContext, SourceBuilderClass,
    SourceBuilderInit, Workspace, compute_build_key, compute_reuse_key,
};
use mbuild_source::SourcePlannedSubject;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
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
    pub(crate) cancellation: CancellationToken,
    pub(crate) realized_inputs: &'a HashMap<BuildKey, RealizedObject>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubjectExecution {
    pub(crate) realized: RealizedObject,
    pub(crate) logger: Arc<dyn BuildLogger>,
}

pub(crate) struct BuilderPlannedSubject {
    builder: &'static dyn Builder,
    name: String,
    config: Value,
    inputs: BTreeMap<String, BuildKey>,
    build_key: BuildKey,
}

impl BuilderPlannedSubject {
    pub(crate) fn new(
        builder: &'static dyn Builder,
        name: String,
        config: Value,
        inputs: BTreeMap<String, BuildKey>,
    ) -> Result<Self, RuntimeError> {
        let tag = builder.tag();
        let spec = builder.spec();
        let reserved_inputs = spec.reserved_input_names().collect::<Vec<_>>();
        for input_name in inputs.keys() {
            if !spec.allow_extra_inputs && !spec.is_reserved_input(input_name) {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' does not accept extra input '{}'; allowed inputs: {}",
                    tag,
                    input_name,
                    reserved_inputs.join(", ")
                )));
            }
        }

        for required in spec.required_inputs {
            if !inputs.contains_key(*required) {
                return Err(RuntimeError::InvalidRequest(format!(
                    "builder '{}' is missing required input '{}' in recipe '{}'",
                    tag, required, name
                )));
            }
        }

        let ordered_direct_deps = spec
            .ordered_present_input_names(&inputs)
            .into_iter()
            .filter_map(|input_name| inputs.get(input_name).copied())
            .collect::<Vec<_>>();
        let build_key =
            compute_build_key(tag, &config, &ordered_direct_deps).map_err(map_identity_error)?;

        Ok(Self {
            builder,
            name,
            config,
            inputs,
            build_key,
        })
    }
}

impl BuilderPlannedSubject {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn build_key(&self) -> BuildKey {
        self.build_key
    }

    pub(crate) fn inputs(&self) -> &BTreeMap<String, BuildKey> {
        &self.inputs
    }
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
    let build_key = subject.build_key;
    let inputs = builder_resolved_inputs(subject, cx.store, cx.realized_inputs)?;
    check_cancelled(&cx.cancellation)?;
    let input_hashes = inputs
        .ordered_reuse_input_hashes(subject.builder.spec())
        .map_err(map_builder_error)?;

    // Resolve the caches before building a workspace: a hit needs no
    // workspace, logger, or temp dir, and is left silent (NoopBuildLogger).
    if let Some(published) = load_build_handle(cx.store, build_key).map_err(map_store_error)? {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &published.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }
    let reuse_key = compute_reuse_key(subject.builder.tag(), &subject.config, &input_hashes)
        .map_err(map_identity_error)?;
    if let Some(published) =
        resolve_reuse_for_build(cx.store, build_key, reuse_key).map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &published.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }

    // Miss: create the workspace and run the builder.
    let workspace = create_workspace(
        cx.store,
        subject.builder.tag(),
        Some(subject.name.clone()),
        build_key.to_string(),
    )
    .map(core_workspace)
    .map_err(map_store_error)?;
    let builder_run = subject.builder.create_object(BuilderRunInit {
        recipe_name: Some(subject.name.clone()),
        build_key: build_key.to_string(),
        workspace,
    });
    // Owns the temp dir from here on: every return path (bind error below, and
    // panics) cleans it via Drop.
    let mut temp_guard = TempDirGuard::for_builder(
        cx.store,
        subject.builder.tag(),
        build_key,
        builder_run.temp_dir().to_path_buf(),
    );
    let logger = cx
        .run_logger
        .bind_builder(&builder_run)
        .map_err(RuntimeError::Store)?;
    temp_guard.set_logger(logger.clone());
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting subject",
    );
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "executing builder",
    );
    check_cancelled(&cx.cancellation)?;
    let mut context = build_context(
        cx.store,
        &builder_run,
        build_key,
        logger.clone(),
        cx.cancellation.clone(),
    )?;
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "run",
        "running builder implementation",
    );
    let staged = subject
        .builder
        .build_erased(
            subject.config.clone(),
            inputs.into_builder_inputs(),
            &mut context,
        )
        .map_err(|error| {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                error.to_string(),
            );
            map_builder_error(error)
        })?;
    check_cancelled(&cx.cancellation)?;
    let published = match staged.object_hash {
        Some(object_hash) => materialize_build_with_trusted_hash(
            cx.store,
            build_key,
            reuse_key,
            input_hashes,
            &staged.staged_path,
            object_hash,
        ),
        None => materialize_build(
            cx.store,
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
    if let Some(published) = load_build_handle(cx.store, build_key).map_err(map_store_error)? {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &published.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }
    if let Some(stored) = record_existing_source_object(cx.store, subject.declared_object_hash())
        .map_err(map_store_error)?
    {
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &stored.object_record),
            logger: Arc::new(NoopBuildLogger),
        });
    }

    // Miss: create the workspace and materialize the source.
    let workspace = create_workspace(
        cx.store,
        "Source",
        Some(subject.name().to_string()),
        build_key.to_string(),
    )
    .map(core_workspace)
    .map_err(map_store_error)?;
    let source_builder = SourceBuilderClass.create_object(SourceBuilderInit {
        recipe_name: subject.name().to_string(),
        build_key: build_key.to_string(),
        declared_object_hash: subject.declared_object_hash(),
        origin: subject.clone_origin(),
        workspace,
    });
    // Owns the temp dir from here on: every return path (bind error below, and
    // panics) cleans it via Drop.
    let mut temp_guard =
        TempDirGuard::for_source(cx.store, source_builder.temp_dir().to_path_buf());
    let logger = cx
        .run_logger
        .bind_source(&source_builder)
        .map_err(RuntimeError::Store)?;
    temp_guard.set_logger(logger.clone());
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting subject",
    );
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "materializing source",
    );

    let Some(origin) = source_builder.origin() else {
        let message = format!(
            "source '{}' has no origin and object '{}' is not present in store",
            source_builder.recipe_name(),
            source_builder.declared_object_hash()
        );
        log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
        return Err(RuntimeError::Build(message));
    };

    let temp_root = source_builder.temp_dir().to_path_buf();
    check_cancelled(&cx.cancellation)?;
    let staged_path = origin
        .materialize(&OriginContext {
            temp_root: temp_root.as_path(),
        })
        .map_err(|error| {
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                error.to_string(),
            );
            RuntimeError::Build(error)
        })?;
    validate_origin_staged_path(&staged_path, &temp_root).map_err(|message| {
        log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
        RuntimeError::Build(message)
    })?;
    check_cancelled(&cx.cancellation)?;
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "run",
        "materializing source origin",
    );

    let import_outcome = import_source_object(
        cx.store,
        source_builder.declared_object_hash(),
        &staged_path,
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
                source_builder.recipe_name(),
                source_builder.declared_object_hash(),
                actual_hash
            );
            log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
            Err(RuntimeError::Build(message))
        }
    }
}

fn validate_origin_staged_path(staged_path: &Path, temp_root: &Path) -> Result<(), String> {
    let canonical_temp_root = temp_root.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize source temp root '{}': {error}",
            temp_root.display()
        )
    })?;
    let canonical_staged_path = staged_path.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize source staged path '{}': {error}",
            staged_path.display()
        )
    })?;
    if !canonical_staged_path.starts_with(&canonical_temp_root) {
        return Err(format!(
            "source origin returned staged path '{}' outside temp root '{}'",
            staged_path.display(),
            temp_root.display()
        ));
    }
    Ok(())
}

fn builder_resolved_inputs(
    subject: &BuilderPlannedSubject,
    store: &Store,
    realized_inputs: &HashMap<BuildKey, RealizedObject>,
) -> Result<ResolvedInputs, RuntimeError> {
    let mut inputs = ResolvedInputs::empty();
    for input_name in subject
        .builder
        .spec()
        .ordered_present_input_names(&subject.inputs)
    {
        let key = *subject.inputs.get(input_name).ok_or_else(|| {
            RuntimeError::Store(format!(
                "planned builder input '{}' is missing for '{}'",
                input_name, subject.name
            ))
        })?;
        let realized = realized_inputs.get(&key).cloned().ok_or_else(|| {
            RuntimeError::Build(format!(
                "dependency object '{}' is not available in completed set",
                key
            ))
        })?;
        inputs.insert(
            input_name,
            ResolvedDependency {
                object_hash: realized.object_hash,
                object_path: store.object_path(realized.object_hash),
            },
        );
    }
    Ok(inputs)
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
    use serde_json::json;
    use std::str::FromStr;

    fn sample_build_key() -> BuildKey {
        BuildKey::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap()
    }

    fn expect_builder_subject_error(
        result: Result<BuilderPlannedSubject, RuntimeError>,
    ) -> RuntimeError {
        match result {
            Ok(_) => panic!("expected builder subject error"),
            Err(error) => error,
        }
    }

    #[test]
    fn builder_subject_rejects_extra_inputs() {
        let builder = crate::builders::get_builder("Tree").unwrap();
        let error = expect_builder_subject_error(BuilderPlannedSubject::new(
            builder,
            "tree".to_string(),
            json!({}),
            BTreeMap::from([("unexpected".to_string(), sample_build_key())]),
        ));

        assert!(
            error
                .to_string()
                .contains("does not accept extra input 'unexpected'"),
            "{error}"
        );
    }

    #[test]
    fn builder_subject_rejects_missing_required_inputs() {
        let builder = crate::builders::get_builder("Sandbox").unwrap();
        let error = expect_builder_subject_error(BuilderPlannedSubject::new(
            builder,
            "sandbox".to_string(),
            json!({}),
            BTreeMap::from([("script".to_string(), sample_build_key())]),
        ));

        assert!(
            error
                .to_string()
                .contains("builder 'Sandbox' is missing required input 'rootfs'"),
            "{error}"
        );
    }

    #[test]
    fn builder_subject_computes_build_key_from_ordered_inputs() {
        let builder = crate::builders::get_builder("Tree").unwrap();
        let config = json!({
            "tree": {
                "entries": [{
                    "type": "file",
                    "path": "hello.txt",
                    "text": "hello",
                    "executable": false
                }]
            }
        });
        let builder_subject = BuilderPlannedSubject::new(
            builder,
            "tree".to_string(),
            config.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        let expected = compute_build_key("Tree", &config, &[]).unwrap();

        assert_eq!(builder_subject.name(), "tree");
        assert_eq!(builder_subject.build_key(), expected);
        assert!(builder_subject.inputs().is_empty());
    }

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
