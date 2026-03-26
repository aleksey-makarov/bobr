use mbuild_core::{
    BuildKey, BuilderError, BuilderInputObject, BuilderInputValue, BuilderInputs, ObjectHash,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedObject {
    pub(crate) object_hash: ObjectHash,
    pub(crate) build_key: BuildKey,
    pub(crate) kind: String,
    pub(crate) object_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedInputs {
    slots: BTreeMap<String, ResolvedInputValue>,
}

impl ResolvedInputs {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn new(slots: BTreeMap<String, ResolvedInputValue>) -> Self {
        Self { slots }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub(crate) fn insert(&mut self, name: impl Into<String>, value: ResolvedInputValue) {
        self.slots.insert(name.into(), value);
    }

    pub(crate) fn one(&self, name: &str) -> Result<&ResolvedObject, BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedInputValue::One(object)) => Ok(object),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    pub(crate) fn optional(&self, name: &str) -> Result<Option<&ResolvedObject>, BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedInputValue::Optional(object)) => Ok(object.as_ref()),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "optional input slot '{name}' is missing"
            ))),
        }
    }

    pub(crate) fn many(&self, name: &str) -> Result<&[ResolvedObject], BuilderError> {
        match self.slots.get(name) {
            Some(ResolvedInputValue::Many(objects)) => Ok(objects),
            Some(_) => Err(BuilderError::ExecutionFailed(format!(
                "input slot '{name}' has unexpected arity"
            ))),
            None => Err(BuilderError::ExecutionFailed(format!(
                "repeated input slot '{name}' is missing"
            ))),
        }
    }

    pub(crate) fn into_builder_inputs(self) -> BuilderInputs {
        let slots = self
            .slots
            .into_iter()
            .map(|(name, value)| {
                let value = match value {
                    ResolvedInputValue::One(object) => BuilderInputValue::One(BuilderInputObject {
                        object_path: object.object_path,
                    }),
                    ResolvedInputValue::Optional(object) => {
                        BuilderInputValue::Optional(object.map(|object| BuilderInputObject {
                            object_path: object.object_path,
                        }))
                    }
                    ResolvedInputValue::Many(objects) => BuilderInputValue::Many(
                        objects
                            .into_iter()
                            .map(|object| BuilderInputObject {
                                object_path: object.object_path,
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
pub(crate) enum ResolvedInputValue {
    One(ResolvedObject),
    Optional(Option<ResolvedObject>),
    Many(Vec<ResolvedObject>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn sample_object() -> ResolvedObject {
        ResolvedObject {
            object_hash: ObjectHash::from_str(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            build_key: BuildKey::from_str(
                "2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap(),
            kind: "build-script".to_string(),
            object_path: PathBuf::from("/tmp/object"),
        }
    }

    #[test]
    fn resolved_inputs_helpers_work() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", ResolvedInputValue::One(object.clone()));
        inputs.insert("base", ResolvedInputValue::Optional(None));
        inputs.insert(
            "sources",
            ResolvedInputValue::Many(vec![object.clone(), object.clone()]),
        );

        assert_eq!(inputs.one("script").unwrap().kind, "build-script");
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
        inputs.insert("script", ResolvedInputValue::One(object));

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
        second.kind = "source-tree".to_string();

        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "sources".to_string(),
            ResolvedInputValue::Many(vec![first.clone(), second.clone()]),
        )]));

        let many = inputs.many("sources").unwrap();
        assert_eq!(many[0].kind, first.kind);
        assert_eq!(many[1].kind, second.kind);
    }

    #[test]
    fn resolved_inputs_new_and_is_empty_work() {
        assert!(ResolvedInputs::empty().is_empty());
        let mut slots = BTreeMap::new();
        slots.insert(
            "script".to_string(),
            ResolvedInputValue::One(sample_object()),
        );
        assert!(!ResolvedInputs::new(slots).is_empty());
    }

    #[test]
    fn resolved_inputs_optional_some_returns_object() {
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "base".to_string(),
            ResolvedInputValue::Optional(Some(object.clone())),
        )]));

        let resolved = inputs.optional("base").unwrap().unwrap();
        assert_eq!(resolved.build_key, object.build_key);
    }

    #[test]
    fn resolved_inputs_convert_to_builder_inputs() {
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([(
            "script".to_string(),
            ResolvedInputValue::One(object.clone()),
        )]));

        let builder_inputs = inputs.into_builder_inputs();
        let resolved = builder_inputs.one("script").unwrap();
        assert_eq!(resolved.object_path, object.object_path);
    }
}
