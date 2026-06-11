use bobr_store::ReuseInputIdentity;
use mbuild_core::{BuilderError, BuilderInputObject, BuilderInputs, InputSpec, ObjectHash};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedDependency {
    pub(crate) object_hash: ObjectHash,
    pub(crate) object_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedInputs {
    slots: BTreeMap<String, ResolvedDependency>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ResolvedInputs {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn new(slots: BTreeMap<String, ResolvedDependency>) -> Self {
        Self { slots }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub(crate) fn insert(&mut self, name: impl Into<String>, value: ResolvedDependency) {
        self.slots.insert(name.into(), value);
    }

    pub(crate) fn required(&self, name: &str) -> Result<&ResolvedDependency, BuilderError> {
        match self.slots.get(name) {
            Some(object) => Ok(object),
            None => Err(BuilderError::ExecutionFailed(format!(
                "required input slot '{name}' is missing"
            ))),
        }
    }

    pub(crate) fn optional(&self, name: &str) -> Option<&ResolvedDependency> {
        self.slots.get(name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&ResolvedDependency> {
        self.slots.get(name)
    }

    pub(crate) fn extra<'a>(
        &'a self,
        spec: &InputSpec,
        name: &str,
    ) -> Option<&'a ResolvedDependency> {
        if spec.is_reserved_input(name) {
            None
        } else {
            self.slots.get(name)
        }
    }

    pub(crate) fn extras<'a>(
        &'a self,
        spec: &'a InputSpec,
    ) -> impl Iterator<Item = (&'a str, &'a ResolvedDependency)> + 'a {
        self.slots.iter().filter_map(move |(name, dep)| {
            if spec.is_reserved_input(name) {
                None
            } else {
                Some((name.as_str(), dep))
            }
        })
    }

    pub(crate) fn ordered_reuse_input_identities(
        &self,
        spec: &InputSpec,
    ) -> Result<Vec<ReuseInputIdentity>, BuilderError> {
        let mut ordered = Vec::new();
        for name in spec.ordered_present_input_names(&self.slots) {
            if let Some(object) = self.get(name) {
                ordered.push(ReuseInputIdentity {
                    object_hash: object.object_hash,
                });
            }
        }
        Ok(ordered)
    }

    pub(crate) fn into_builder_inputs(self) -> BuilderInputs {
        let slots = self
            .slots
            .into_iter()
            .map(|(name, value)| {
                let value = BuilderInputObject {
                    path: value.object_path,
                };
                (name, value)
            })
            .collect();
        BuilderInputs::new(slots)
    }
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
            object_path: PathBuf::from("/tmp/object"),
        }
    }

    #[test]
    fn resolved_inputs_helpers_work() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", object.clone());
        inputs.insert("source", object.clone());

        let spec = InputSpec {
            required_inputs: &["rootfs"],
            optional_inputs: &["base"],
            allow_extra_inputs: true,
        };

        assert_eq!(
            inputs.required("script").unwrap().object_path,
            object.object_path
        );
        assert!(inputs.optional("base").is_none());
        assert!(inputs.extra(&spec, "source").is_some());
        assert_eq!(inputs.extras(&spec).count(), 2);
    }

    #[test]
    fn resolved_inputs_missing_required_input_is_an_error() {
        let object = sample_object();
        let mut inputs = ResolvedInputs::empty();
        inputs.insert("script", object);

        assert!(matches!(
            inputs.required("missing"),
            Err(BuilderError::ExecutionFailed(_))
        ));
        assert!(inputs.optional("missing").is_none());
    }

    #[test]
    fn resolved_inputs_extras_follow_lexical_order() {
        let first = sample_object();
        let second = sample_object();

        let spec = InputSpec {
            required_inputs: &["rootfs"],
            optional_inputs: &[],
            allow_extra_inputs: true,
        };
        let inputs = ResolvedInputs::new(BTreeMap::from([
            ("source_b".to_string(), second.clone()),
            ("source_a".to_string(), first.clone()),
        ]));

        let extras = inputs.extras(&spec).collect::<Vec<_>>();
        assert_eq!(extras[0].0, "source_a");
        assert_eq!(extras[0].1.object_path, first.object_path);
        assert_eq!(extras[1].0, "source_b");
        assert_eq!(extras[1].1.object_path, second.object_path);
    }

    #[test]
    fn resolved_inputs_new_and_is_empty_work() {
        assert!(ResolvedInputs::empty().is_empty());
        let mut slots = BTreeMap::new();
        slots.insert("script".to_string(), sample_object());
        assert!(!ResolvedInputs::new(slots).is_empty());
    }

    #[test]
    fn resolved_inputs_optional_some_returns_object() {
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([("base".to_string(), object.clone())]));

        let resolved = inputs.optional("base").unwrap();
        assert_eq!(resolved.object_path, object.object_path);
    }

    #[test]
    fn resolved_inputs_convert_to_builder_inputs() {
        let object = sample_object();
        let inputs = ResolvedInputs::new(BTreeMap::from([("script".to_string(), object.clone())]));

        let builder_inputs = inputs.into_builder_inputs();
        let resolved = builder_inputs.required("script").unwrap();
        assert_eq!(resolved.path, object.object_path);
    }

    static ORDERED_SPEC: InputSpec = InputSpec {
        required_inputs: &["first"],
        optional_inputs: &["optional"],
        allow_extra_inputs: true,
    };

    #[test]
    fn ordered_reuse_input_identities_follow_input_spec_order() {
        let mut first = sample_object();
        first.object_hash = ObjectHash::from_str(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let mut optional = sample_object();
        optional.object_hash = ObjectHash::from_str(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        let mut extra_a = sample_object();
        extra_a.object_hash = ObjectHash::from_str(
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        let mut extra_b = sample_object();
        extra_b.object_hash = ObjectHash::from_str(
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        )
        .unwrap();

        let inputs = ResolvedInputs::new(BTreeMap::from([
            ("first".to_string(), first),
            ("optional".to_string(), optional),
            ("extra_b".to_string(), extra_b),
            ("extra_a".to_string(), extra_a),
        ]));

        let ordered = inputs
            .ordered_reuse_input_identities(&ORDERED_SPEC)
            .unwrap();
        assert_eq!(
            ordered,
            vec![
                ReuseInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    )
                    .unwrap(),
                },
                ReuseInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    )
                    .unwrap(),
                },
                ReuseInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    )
                    .unwrap(),
                },
                ReuseInputIdentity {
                    object_hash: ObjectHash::from_str(
                        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    )
                    .unwrap(),
                },
            ]
        );
    }
}
