use mbuild_core::{
    BuildKey, BuilderError, BuilderInputObject, BuilderInputValue, BuilderInputs, BuilderSpec,
    InputArity, MetaHash, ObjectHash, ResultInputIdentity,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedDependency {
    pub(crate) object_hash: ObjectHash,
    pub(crate) meta_hash: MetaHash,
    pub(crate) build_key: BuildKey,
    pub(crate) object_path: PathBuf,
    pub(crate) meta: Map<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedInputs {
    slots: BTreeMap<String, ResolvedDependencyValue>,
}

impl ResolvedInputs {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn new(slots: BTreeMap<String, ResolvedDependencyValue>) -> Self {
        Self { slots }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub(crate) fn insert(&mut self, name: impl Into<String>, value: ResolvedDependencyValue) {
        self.slots.insert(name.into(), value);
    }

    pub(crate) fn one(&self, name: &str) -> Result<&ResolvedDependency, BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedDependencyValue::One(object)) => Ok(object),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    pub(crate) fn optional(&self, name: &str) -> Result<Option<&ResolvedDependency>, BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedDependencyValue::Optional(object)) => Ok(object.as_ref()),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "optional input slot '{name}' is missing"
            ))),
        }
    }

    pub(crate) fn many(&self, name: &str) -> Result<&[ResolvedDependency], BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedDependencyValue::Many(objects)) => Ok(objects),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "repeated input slot '{name}' is missing"
            ))),
        }
    }

    pub(crate) fn ordered_build_keys(
        &self,
        spec: &BuilderSpec,
    ) -> Result<Vec<BuildKey>, BuilderError> {
        let mut ordered = Vec::new();
        for slot in spec.inputs {
            match slot.arity {
                InputArity::One => ordered.push(self.one(slot.name)?.build_key),
                InputArity::Optional => {
                    if let Some(object) = self.optional(slot.name)? {
                        ordered.push(object.build_key);
                    }
                }
                InputArity::Many => {
                    ordered.extend(self.many(slot.name)?.iter().map(|object| object.build_key));
                }
            }
        }
        Ok(ordered)
    }

    pub(crate) fn ordered_input_identities(
        &self,
        spec: &BuilderSpec,
    ) -> Result<Vec<ResultInputIdentity>, BuilderError> {
        let mut ordered = Vec::new();
        for slot in spec.inputs {
            match slot.arity {
                InputArity::One => {
                    let object = self.one(slot.name)?;
                    ordered.push(ResultInputIdentity {
                        object_hash: object.object_hash,
                        meta_hash: object.meta_hash,
                    });
                }
                InputArity::Optional => {
                    if let Some(object) = self.optional(slot.name)? {
                        ordered.push(ResultInputIdentity {
                            object_hash: object.object_hash,
                            meta_hash: object.meta_hash,
                        });
                    }
                }
                InputArity::Many => {
                    ordered.extend(self.many(slot.name)?.iter().map(|object| {
                        ResultInputIdentity {
                            object_hash: object.object_hash,
                            meta_hash: object.meta_hash,
                        }
                    }));
                }
            }
        }
        Ok(ordered)
    }

    pub(crate) fn into_builder_inputs(self) -> BuilderInputs {
        let slots = self
            .slots
            .into_iter()
            .map(|(name, value)| {
                let value = match value {
                    ResolvedDependencyValue::One(object) => {
                        BuilderInputValue::One(BuilderInputObject {
                            object_path: object.object_path,
                            meta: object.meta,
                        })
                    }
                    ResolvedDependencyValue::Optional(object) => {
                        BuilderInputValue::Optional(object.map(|object| BuilderInputObject {
                            object_path: object.object_path,
                            meta: object.meta,
                        }))
                    }
                    ResolvedDependencyValue::Many(objects) => BuilderInputValue::Many(
                        objects
                            .into_iter()
                            .map(|object| BuilderInputObject {
                                object_path: object.object_path,
                                meta: object.meta,
                            })
                            .collect(),
                    ),
                };
                (name, value)
            })
            .collect();
        BuilderInputs::new(slots)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ResolvedDependencyValue {
    One(ResolvedDependency),
    Optional(Option<ResolvedDependency>),
    Many(Vec<ResolvedDependency>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn sample_object() -> ResolvedDependency {
        ResolvedDependency {
            object_hash: ObjectHash::from_str(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            meta_hash: MetaHash::from_str(
                "3333333333333333333333333333333333333333333333333333333333333333",
            )
            .unwrap(),
            build_key: BuildKey::from_str(
                "2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap(),
            object_path: PathBuf::from("/tmp/object"),
            meta: Map::new(),
        }
    }

    #[test]
    fn resolved_inputs_helpers_work() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", ResolvedDependencyValue::One(object.clone()));
        inputs.insert("base", ResolvedDependencyValue::Optional(None));
        inputs.insert(
            "sources",
            ResolvedDependencyValue::Many(vec![object.clone(), object.clone()]),
        );

        assert!(inputs.one("script").unwrap().meta.is_empty());
        assert!(inputs.optional("base").unwrap().is_none());
        assert_eq!(inputs.many("sources").unwrap().len(), 2);
        assert!(matches!(
            inputs.one("sources"),
            Err(BuilderError::ExecutionFailed(_))
        ));
    }

    #[test]
    fn resolved_inputs_missing_and_wrong_arity_are_errors() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", ResolvedDependencyValue::One(object));

        assert!(matches!(
            inputs.optional("script"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.many("script"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.one("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.optional("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(matches!(
            inputs.many("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
    }

    #[test]
    fn resolved_inputs_many_preserves_order() {
        let first = sample_object();
        let mut second = sample_object();
        second.meta.insert(
            "install".to_string(),
            serde_json::json!({"rules":[{"path":"**","attrs":{"uid":0,"gid":0,"directory_mode":493,"regular_file_mode":420,"executable_file_mode":493,"symlink_mode":511}}]}),
        );

        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "sources".to_string(),
            ResolvedDependencyValue::Many(vec![first.clone(), second.clone()]),
        )]));

        let many = inputs.many("sources").unwrap();
        assert_eq!(many[0].meta, first.meta);
        assert_eq!(many[1].meta, second.meta);
    }

    #[test]
    fn resolved_inputs_new_and_is_empty_work() {
        assert!(ResolvedInputs::empty().is_empty());
        let mut slots = BTreeMap::new();
        slots.insert(
            "script".to_string(),
            ResolvedDependencyValue::One(sample_object()),
        );
        assert!(!ResolvedInputs::new(slots).is_empty());
    }

    #[test]
    fn resolved_inputs_optional_some_returns_object() {
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "base".to_string(),
            ResolvedDependencyValue::Optional(Some(object.clone())),
        )]));

        let resolved = inputs.optional("base").unwrap().unwrap();
        assert_eq!(resolved.build_key, object.build_key);
    }

    #[test]
    fn resolved_inputs_convert_to_builder_inputs() {
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "script".to_string(),
            ResolvedDependencyValue::One(object.clone()),
        )]));

        let builder_inputs = inputs.into_builder_inputs();
        let resolved = builder_inputs.one("script").unwrap();
        assert_eq!(resolved.object_path, object.object_path);
        assert_eq!(resolved.meta, object.meta);
    }

    static ORDERED_SPEC: BuilderSpec = BuilderSpec {
        tag: "Ordered",
        inputs: &[
            mbuild_core::InputSlot {
                name: "first",
                arity: InputArity::One,
            },
            mbuild_core::InputSlot {
                name: "optional",
                arity: InputArity::Optional,
            },
            mbuild_core::InputSlot {
                name: "many",
                arity: InputArity::Many,
            },
        ],
    };

    #[test]
    fn ordered_build_keys_follow_builder_spec_order() {
        let mut first = sample_object();
        first.build_key =
            BuildKey::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();
        let mut optional = sample_object();
        optional.build_key =
            BuildKey::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .unwrap();
        let mut many_a = sample_object();
        many_a.build_key =
            BuildKey::from_str("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
                .unwrap();
        let mut many_b = sample_object();
        many_b.build_key =
            BuildKey::from_str("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd")
                .unwrap();

        let inputs = ResolvedInputs::new(BTreeMap::from([
            ("first".to_string(), ResolvedDependencyValue::One(first)),
            (
                "optional".to_string(),
                ResolvedDependencyValue::Optional(Some(optional)),
            ),
            (
                "many".to_string(),
                ResolvedDependencyValue::Many(vec![many_a, many_b]),
            ),
        ]));

        let ordered = inputs.ordered_build_keys(&ORDERED_SPEC).unwrap();
        assert_eq!(
            ordered,
            vec![
                BuildKey::from_str(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap(),
                BuildKey::from_str(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )
                .unwrap(),
                BuildKey::from_str(
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                )
                .unwrap(),
                BuildKey::from_str(
                    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                )
                .unwrap(),
            ]
        );
    }

    #[test]
    fn ordered_input_identities_follow_builder_spec_order() {
        let mut first = sample_object();
        first.object_hash = ObjectHash::from_str(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        first.meta_hash =
            MetaHash::from_str("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();
        let mut optional = sample_object();
        optional.object_hash = ObjectHash::from_str(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        optional.meta_hash =
            MetaHash::from_str("2222222222222222222222222222222222222222222222222222222222222222")
                .unwrap();
        let mut many_a = sample_object();
        many_a.object_hash = ObjectHash::from_str(
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        many_a.meta_hash =
            MetaHash::from_str("3333333333333333333333333333333333333333333333333333333333333333")
                .unwrap();
        let mut many_b = sample_object();
        many_b.object_hash = ObjectHash::from_str(
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        )
        .unwrap();
        many_b.meta_hash =
            MetaHash::from_str("4444444444444444444444444444444444444444444444444444444444444444")
                .unwrap();

        let inputs = ResolvedInputs::new(BTreeMap::from([
            ("first".to_string(), ResolvedDependencyValue::One(first)),
            (
                "optional".to_string(),
                ResolvedDependencyValue::Optional(Some(optional)),
            ),
            (
                "many".to_string(),
                ResolvedDependencyValue::Many(vec![many_a, many_b]),
            ),
        ]));

        let ordered = inputs.ordered_input_identities(&ORDERED_SPEC).unwrap();
        assert_eq!(
            ordered,
            vec![
                ResultInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    )
                    .unwrap(),
                    meta_hash: MetaHash::from_str(
                        "1111111111111111111111111111111111111111111111111111111111111111",
                    )
                    .unwrap(),
                },
                ResultInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    )
                    .unwrap(),
                    meta_hash: MetaHash::from_str(
                        "2222222222222222222222222222222222222222222222222222222222222222",
                    )
                    .unwrap(),
                },
                ResultInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    )
                    .unwrap(),
                    meta_hash: MetaHash::from_str(
                        "3333333333333333333333333333333333333333333333333333333333333333",
                    )
                    .unwrap(),
                },
                ResultInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    )
                    .unwrap(),
                    meta_hash: MetaHash::from_str(
                        "4444444444444444444444444444444444444444444444444444444444444444",
                    )
                    .unwrap(),
                },
            ]
        );
    }
}
