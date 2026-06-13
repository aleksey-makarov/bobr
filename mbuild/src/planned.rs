use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use crate::runtime::{
    ExecuteBuilderNodeRequest, RuntimeError, check_cancelled, execute_builder_node,
    log_runtime_event, map_identity_error, map_store_error,
};
use bobr_store::{
    ObjectRecord, RealizedObject, SourceImportOutcome, Store, create_workspace,
    import_source_object, load_build_handle, record_existing_source_object,
    remove_store_temp_dir_force,
};
use mbuild_core::{
    BuildKey, BuildLogLevel, BuildLogger, BuildRunLogger, Builder, BuilderClassBase,
    CancellationToken, OriginContext, SourceBuilderClass, SourceBuilderInit, Workspace,
    compute_build_key,
};
use mbuild_source::SourcePlannedSubject;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
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
    let inputs = builder_resolved_inputs(subject, cx.store, cx.realized_inputs)?;
    let executed = execute_builder_node(ExecuteBuilderNodeRequest {
        store: cx.store,
        builder: subject.builder,
        build_key: subject.build_key,
        build_name: &subject.name,
        run_logger: cx.run_logger,
        cancellation: cx.cancellation,
        config: subject.config.clone(),
        inputs,
    })?;
    Ok(SubjectExecution {
        realized: realized_object_from_record(
            Some(subject.build_key),
            &executed.published.object_record,
        ),
        logger: executed.logger,
    })
}

fn execute_source_subject(
    subject: &SourcePlannedSubject,
    cx: PlannedExecutionContext<'_>,
) -> Result<SubjectExecution, RuntimeError> {
    let build_key = subject.build_key();
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
    let logger = cx
        .run_logger
        .bind_source(&source_builder)
        .map_err(RuntimeError::Store)?;
    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "start",
        "starting builder node",
    );
    if let Err(error) = check_cancelled(&cx.cancellation) {
        cleanup_workspace_temp_dir(cx.store, source_builder.temp_dir(), logger.as_ref());
        return Err(error);
    }

    if let Some(published) = load_build_handle(cx.store, build_key).map_err(map_store_error)? {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "cache-hit",
            "reusing existing source build ref",
        );
        cleanup_workspace_temp_dir(cx.store, source_builder.temp_dir(), logger.as_ref());
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &published.object_record),
            logger,
        });
    }

    if let Some(stored) =
        record_existing_source_object(cx.store, source_builder.declared_object_hash())
            .map_err(map_store_error)?
    {
        log_runtime_event(
            logger.as_ref(),
            BuildLogLevel::Info,
            "object-hit",
            "reusing existing source object",
        );
        cleanup_workspace_temp_dir(cx.store, source_builder.temp_dir(), logger.as_ref());
        return Ok(SubjectExecution {
            realized: realized_object_from_record(Some(build_key), &stored.object_record),
            logger,
        });
    }

    log_runtime_event(
        logger.as_ref(),
        BuildLogLevel::Info,
        "cache-miss",
        "materializing source",
    );

    if source_builder.origin().is_none() {
        let message = format!(
            "source '{}' has no origin and object '{}' is not present in store",
            source_builder.recipe_name(),
            source_builder.declared_object_hash()
        );
        log_runtime_event(logger.as_ref(), BuildLogLevel::Error, "fail", &message);
        cleanup_workspace_temp_dir(cx.store, source_builder.temp_dir(), logger.as_ref());
        return Err(RuntimeError::Build(message));
    }

    let temp_root = source_builder.temp_dir().to_path_buf();
    if let Err(error) = check_cancelled(&cx.cancellation) {
        cleanup_workspace_temp_dir(cx.store, &temp_root, logger.as_ref());
        return Err(error);
    }
    let staged_path = match source_builder
        .origin()
        .expect("origin checked above")
        .materialize(&OriginContext {
            temp_root: temp_root.as_path(),
        }) {
        Ok(path) => path,
        Err(error) => {
            cleanup_workspace_temp_dir(cx.store, &temp_root, logger.as_ref());
            log_runtime_event(
                logger.as_ref(),
                BuildLogLevel::Error,
                "fail",
                error.to_string(),
            );
            return Err(RuntimeError::Build(error));
        }
    };
    if let Err(error) = check_cancelled(&cx.cancellation) {
        cleanup_workspace_temp_dir(cx.store, &temp_root, logger.as_ref());
        return Err(error);
    }
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
    .map_err(|error| {
        cleanup_workspace_temp_dir(cx.store, &temp_root, logger.as_ref());
        map_store_error(error)
    })?;
    if let Err(error) = check_cancelled(&cx.cancellation) {
        cleanup_workspace_temp_dir(cx.store, &temp_root, logger.as_ref());
        return Err(error);
    }
    cleanup_workspace_temp_dir(cx.store, &temp_root, logger.as_ref());

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

fn core_workspace(workspace: bobr_store::StoreWorkspace) -> Workspace {
    Workspace::new(
        workspace.log_dir().to_path_buf(),
        workspace.raw_log_dir().to_path_buf(),
        workspace.temp_dir().to_path_buf(),
    )
}

fn cleanup_workspace_temp_dir(store: &Store, temp_dir: &std::path::Path, logger: &dyn BuildLogger) {
    if let Err(error) = remove_store_temp_dir_force(store, temp_dir) {
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
