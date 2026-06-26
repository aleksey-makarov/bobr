use bobr_builder::{Builder, BuilderPlanError, BuilderPlannedSubject};
use bobr_core::BuildKey;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Iterates every builder registered in the system (in-tree + sandbox).
///
/// This is the single place that knows all builder sources.
fn registered_builders() -> impl Iterator<Item = &'static dyn Builder> {
    bobr_builder::BUILDERS
        .iter()
        .copied()
        .chain(bobr_sandbox::BUILDERS.iter().copied())
}

/// Parses and plans one builder recipe object against the registered builders.
pub(crate) fn parse_subject(
    tag: &str,
    mut object: Map<String, Value>,
    inputs: BTreeMap<String, BuildKey>,
) -> Result<BuilderPlannedSubject, BuilderPlanError> {
    let name = take_string(&mut object, "name")?;
    let config = object
        .remove("config")
        .ok_or_else(|| BuilderPlanError::recipe("missing required field 'config'"))?;
    if !object.is_empty() {
        return Err(BuilderPlanError::recipe(format!(
            "unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    let builder = registered_builders()
        .find(|builder| builder.tag().eq_ignore_ascii_case(tag))
        .ok_or_else(|| BuilderPlanError::UnknownBuilder {
            tag: tag.to_string(),
            supported_tags: registered_builders().map(|builder| builder.tag()).collect(),
        })?;
    BuilderPlannedSubject::new(builder, name, config, inputs)
}

fn take_string(object: &mut Map<String, Value>, field: &str) -> Result<String, BuilderPlanError> {
    let value = object
        .remove(field)
        .ok_or_else(|| BuilderPlanError::recipe(format!("missing required field '{field}'")))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| BuilderPlanError::recipe(format!("{field}: expected string")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::str::FromStr;

    fn object_of(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn registered_builders_are_well_formed() {
        let mut seen = BTreeSet::new();
        for builder in registered_builders() {
            assert!(
                seen.insert(builder.tag().to_ascii_lowercase()),
                "duplicate builder tag '{}'",
                builder.tag()
            );
            builder
                .spec()
                .validate_for_builder(builder.tag())
                .unwrap_or_else(|error| panic!("invalid spec for '{}': {error}", builder.tag()));
        }
    }

    #[test]
    fn parse_subject_reports_unknown_builder() {
        let error = parse_subject(
            "Missing",
            object_of(json!({"name": "node", "config": {}})),
            BTreeMap::new(),
        )
        .unwrap_err();

        assert!(matches!(error, BuilderPlanError::UnknownBuilder { .. }));
        assert!(error.to_string().contains("unknown builder tag 'Missing'"));
    }

    #[test]
    fn parse_subject_requires_name_and_config() {
        let missing_name =
            parse_subject("Tree", object_of(json!({"config": {}})), BTreeMap::new()).unwrap_err();
        let missing_config =
            parse_subject("Tree", object_of(json!({"name": "node"})), BTreeMap::new()).unwrap_err();

        assert_eq!(missing_name.to_string(), "missing required field 'name'");
        assert_eq!(
            missing_config.to_string(),
            "missing required field 'config'"
        );
    }

    #[test]
    fn parse_subject_rejects_unexpected_fields() {
        let error = parse_subject(
            "Tree",
            object_of(json!({"name": "node", "config": {}, "extra": true})),
            BTreeMap::new(),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "unexpected fields: extra");
    }

    #[test]
    fn parse_subject_rejects_extra_inputs() {
        let input_key =
            BuildKey::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();

        let error = parse_subject(
            "Tree",
            object_of(json!({"name": "node", "config": {}})),
            BTreeMap::from([("unexpected".to_string(), input_key)]),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not accept extra input 'unexpected'"),
            "{error}"
        );
    }

    #[test]
    fn parse_subject_rejects_invalid_actual_input_names() {
        let input_key =
            BuildKey::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();

        let error = parse_subject(
            "Group",
            object_of(json!({"name": "group", "config": {}})),
            BTreeMap::from([("bad-name".to_string(), input_key)]),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid input 'bad-name': input name 'bad-name'"),
            "{error}"
        );
    }

    #[test]
    fn parse_subject_rejects_missing_required_inputs() {
        let error = parse_subject(
            "TreeSubset",
            object_of(json!({"name": "node", "config": {}})),
            BTreeMap::new(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("builder 'TreeSubset' is missing required input 'tree'"),
            "{error}"
        );
    }

    #[test]
    fn parse_subject_computes_build_key_from_ordered_inputs() {
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
        let subject = parse_subject(
            "Tree",
            object_of(json!({"name": "tree", "config": config.clone()})),
            BTreeMap::new(),
        )
        .unwrap();
        let expected = bobr_core::compute_build_key("Tree", &config, &BTreeMap::new()).unwrap();

        assert_eq!(subject.name(), "tree");
        assert_eq!(subject.tag(), "Tree");
        assert_eq!(subject.build_key(), expected);
        assert!(subject.inputs().is_empty());
    }
}
