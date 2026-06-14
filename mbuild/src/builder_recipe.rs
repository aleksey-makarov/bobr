use crate::planned::BuilderPlannedSubject;
use crate::runtime::RuntimeError;
use mbuild_core::{BuildKey, validate_publication_name};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(crate) fn parse_builder_subject(
    tag: &str,
    mut object: Map<String, Value>,
    inputs: BTreeMap<String, BuildKey>,
    path: &str,
) -> Result<BuilderPlannedSubject, RuntimeError> {
    let name = take_string(&mut object, path, "name")?;
    validate_publication_name(&name)
        .map_err(|error| RuntimeError::RecipeLoad(format!("{path}.name: {error}")))?;
    let config = object.remove("config").ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: missing required field 'config'"))
    })?;
    if !object.is_empty() {
        return Err(RuntimeError::RecipeLoad(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    let builder = mbuild_builder::get_builder(tag).ok_or_else(|| {
        RuntimeError::UnknownBuilder(format!(
            "unknown builder tag '{}'; supported builders: {}",
            tag,
            mbuild_builder::supported_builder_tags().join(", ")
        ))
    })?;
    BuilderPlannedSubject::new(builder, name, config, inputs)
}

fn take_string(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<String, RuntimeError> {
    let value = object.remove(field).ok_or_else(|| {
        RuntimeError::RecipeLoad(format!("{path}: missing required field '{field}'"))
    })?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{path}.{field}: expected string")))
}
