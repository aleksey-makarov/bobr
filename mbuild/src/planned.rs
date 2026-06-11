use crate::resolved_inputs::{ResolvedDependency, ResolvedInputs};
use crate::runtime::{
    ExecuteBuilderNodeRequest, RuntimeError, check_cancelled, execute_builder_node,
    log_runtime_event, lookup_build_handle, lookup_canonical_object, map_store_error,
};
use bobr_store::identity::{BuildKey, compute_build_key};
use bobr_store::{
    ObjectRecord, RealizedObject, SourceImportOutcome, SourceLookup, Store, create_workspace,
    import_source_object, lookup_source_object, remove_store_temp_dir_force,
};
use mbuild_core::{
    BuildLogLevel, BuildLogger, BuildRunLogger, Builder, BuilderClassBase, CancellationToken,
    OriginContext, ParsedOrigin, SourceBuilderClass, SourceBuilderInit, Workspace,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub(crate) trait PlannedSubject: Send + Sync {
    fn name(&self) -> &str;
    fn tag(&self) -> &str;
    fn build_key(&self) -> BuildKey;
    fn inputs(&self) -> &BTreeMap<String, BuildKey>;

    fn lookup_direct_reuse(
        &self,
        cx: PlannedLookupContext<'_>,
    ) -> Result<Option<ReuseDecision>, RuntimeError>;

    fn lookup_after_inputs_reused(
        &self,
        cx: PlannedDependencyLookupContext<'_>,
    ) -> Result<Option<ReuseDecision>, RuntimeError>;

    fn execute(&self, cx: PlannedExecutionContext<'_>) -> Result<SubjectExecution, RuntimeError>;
}

pub(crate) struct PlannedLookupContext<'a> {
    pub(crate) store: &'a Store,
}

pub(crate) struct PlannedDependencyLookupContext<'a> {
    pub(crate) store: &'a Store,
    pub(crate) realized_inputs: &'a HashMap<BuildKey, RealizedObject>,
}

pub(crate) struct PlannedExecutionContext<'a> {
    pub(crate) store: &'a Store,
    pub(crate) run_logger: Arc<BuildRunLogger>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) realized_inputs: &'a HashMap<BuildKey, RealizedObject>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReuseDecision {
    pub(crate) realized: RealizedObject,
    pub(crate) origin: ReuseOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReuseOrigin {
    BuildHandle,
    CanonicalObject,
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
            compute_build_key(tag, &config, &ordered_direct_deps).map_err(map_store_error)?;

        Ok(Self {
            builder,
            name,
            config,
            inputs,
            build_key,
        })
    }

    fn realized_input_identities(
        &self,
        realized_inputs: &HashMap<BuildKey, RealizedObject>,
    ) -> Result<Vec<bobr_store::ReuseInputIdentity>, RuntimeError> {
        let mut ordered = Vec::new();
        for input_name in self
            .builder
            .spec()
            .ordered_present_input_names(&self.inputs)
        {
            let key = self.inputs.get(input_name).ok_or_else(|| {
                RuntimeError::Store(format!(
                    "planned builder input '{}' is missing for '{}'",
                    input_name, self.name
                ))
            })?;
            let realized = realized_inputs.get(key).ok_or_else(|| {
                RuntimeError::Build(format!(
                    "dependency object '{}' is not available for '{}'",
                    key, self.name
                ))
            })?;
            ordered.push(bobr_store::ReuseInputIdentity {
                object_hash: realized.object_hash,
            });
        }
        Ok(ordered)
    }

    fn resolved_inputs(
        &self,
        store: &Store,
        realized_inputs: &HashMap<BuildKey, RealizedObject>,
    ) -> Result<ResolvedInputs, RuntimeError> {
        let mut inputs = ResolvedInputs::empty();
        for input_name in self
            .builder
            .spec()
            .ordered_present_input_names(&self.inputs)
        {
            let key = *self.inputs.get(input_name).ok_or_else(|| {
                RuntimeError::Store(format!(
                    "planned builder input '{}' is missing for '{}'",
                    input_name, self.name
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
}

impl PlannedSubject for BuilderPlannedSubject {
    fn name(&self) -> &str {
        &self.name
    }

    fn tag(&self) -> &str {
        self.builder.tag()
    }

    fn build_key(&self) -> BuildKey {
        self.build_key
    }

    fn inputs(&self) -> &BTreeMap<String, BuildKey> {
        &self.inputs
    }

    fn lookup_direct_reuse(
        &self,
        cx: PlannedLookupContext<'_>,
    ) -> Result<Option<ReuseDecision>, RuntimeError> {
        Ok(
            lookup_build_handle(cx.store, self.build_key)?.map(|published| ReuseDecision {
                realized: realized_object_from_record(
                    Some(published.build.build_key),
                    &published.object_record,
                ),
                origin: ReuseOrigin::BuildHandle,
            }),
        )
    }

    fn lookup_after_inputs_reused(
        &self,
        cx: PlannedDependencyLookupContext<'_>,
    ) -> Result<Option<ReuseDecision>, RuntimeError> {
        let input_identities = self.realized_input_identities(cx.realized_inputs)?;
        Ok(lookup_canonical_object(
            cx.store,
            self.builder.tag(),
            &self.config,
            &input_identities,
            self.build_key,
        )?
        .map(|published| ReuseDecision {
            realized: realized_object_from_record(Some(self.build_key), &published.object_record),
            origin: ReuseOrigin::CanonicalObject,
        }))
    }

    fn execute(&self, cx: PlannedExecutionContext<'_>) -> Result<SubjectExecution, RuntimeError> {
        let inputs = self.resolved_inputs(cx.store, cx.realized_inputs)?;
        let executed = execute_builder_node(ExecuteBuilderNodeRequest {
            store: cx.store,
            builder: self.builder,
            build_key: self.build_key,
            build_name: &self.name,
            run_logger: cx.run_logger,
            cancellation: cx.cancellation,
            config: self.config.clone(),
            inputs,
        })?;
        Ok(SubjectExecution {
            realized: realized_object_from_record(
                Some(self.build_key),
                &executed.published.object_record,
            ),
            logger: executed.logger,
        })
    }
}

pub(crate) struct SourcePlannedSubject {
    name: String,
    object_hash: fsobj_hash::ObjectHash,
    origin: Option<Box<dyn ParsedOrigin>>,
    inputs: BTreeMap<String, BuildKey>,
    build_key: BuildKey,
}

impl SourcePlannedSubject {
    pub(crate) fn new(
        name: String,
        object_hash: fsobj_hash::ObjectHash,
        origin: Option<Box<dyn ParsedOrigin>>,
    ) -> Result<Self, RuntimeError> {
        let build_key = source_planning_key(object_hash)?;
        Ok(Self {
            name,
            object_hash,
            origin,
            inputs: BTreeMap::new(),
            build_key,
        })
    }
}

impl PlannedSubject for SourcePlannedSubject {
    fn name(&self) -> &str {
        &self.name
    }

    fn tag(&self) -> &str {
        "Source"
    }

    fn build_key(&self) -> BuildKey {
        self.build_key
    }

    fn inputs(&self) -> &BTreeMap<String, BuildKey> {
        &self.inputs
    }

    fn lookup_direct_reuse(
        &self,
        cx: PlannedLookupContext<'_>,
    ) -> Result<Option<ReuseDecision>, RuntimeError> {
        match lookup_source_object(cx.store, self.object_hash).map_err(map_store_error)? {
            SourceLookup::Hit(stored) => Ok(Some(ReuseDecision {
                realized: realized_object_from_record(None, &stored.object_record),
                origin: ReuseOrigin::CanonicalObject,
            })),
            SourceLookup::Missing => Ok(None),
        }
    }

    fn lookup_after_inputs_reused(
        &self,
        _cx: PlannedDependencyLookupContext<'_>,
    ) -> Result<Option<ReuseDecision>, RuntimeError> {
        Ok(None)
    }

    fn execute(&self, cx: PlannedExecutionContext<'_>) -> Result<SubjectExecution, RuntimeError> {
        let workspace = create_workspace(
            cx.store,
            "Source",
            Some(self.name.clone()),
            self.build_key.to_string(),
        )
        .map(core_workspace)
        .map_err(map_store_error)?;
        let source_builder = SourceBuilderClass.create_object(SourceBuilderInit {
            recipe_name: self.name.clone(),
            build_key: self.build_key.to_string(),
            declared_object_hash: self.object_hash,
            origin: self.origin.clone(),
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

        match lookup_source_object(cx.store, source_builder.declared_object_hash())
            .map_err(map_store_error)?
        {
            SourceLookup::Hit(stored) => {
                log_runtime_event(
                    logger.as_ref(),
                    BuildLogLevel::Info,
                    "object-hit",
                    "reusing existing source object",
                );
                cleanup_workspace_temp_dir(cx.store, source_builder.temp_dir(), logger.as_ref());
                return Ok(SubjectExecution {
                    realized: realized_object_from_record(None, &stored.object_record),
                    logger,
                });
            }
            SourceLookup::Missing => {}
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
                realized: realized_object_from_record(None, &stored.object_record),
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
}

pub(crate) fn source_planning_key(
    object_hash: fsobj_hash::ObjectHash,
) -> Result<BuildKey, RuntimeError> {
    compute_build_key(
        "SourceNode",
        &json!({
            "object_hash": object_hash.to_string(),
        }),
        &[],
    )
    .map_err(map_store_error)
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
        let subject = BuilderPlannedSubject::new(
            builder,
            "tree".to_string(),
            config.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        let expected = compute_build_key("Tree", &config, &[]).unwrap();

        assert_eq!(subject.name(), "tree");
        assert_eq!(subject.tag(), "Tree");
        assert_eq!(subject.build_key(), expected);
        assert!(subject.inputs().is_empty());
    }

    #[test]
    fn source_subject_has_empty_inputs_and_source_build_key() {
        let object_hash = fsobj_hash::ObjectHash::from_str(
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let subject = SourcePlannedSubject::new("source".to_string(), object_hash, None).unwrap();
        let expected = source_planning_key(object_hash).unwrap();

        assert_eq!(subject.name(), "source");
        assert_eq!(subject.tag(), "Source");
        assert_eq!(subject.build_key(), expected);
        assert!(subject.inputs().is_empty());
    }
}
