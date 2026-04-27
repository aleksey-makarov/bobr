mod path;

use crate::runtime::RuntimeError;
use mbuild_core::{OriginHandler, ParsedOrigin};
use serde_json::Value;

use self::path::PathOriginHandler;

static PATH_ORIGIN: PathOriginHandler = PathOriginHandler;

pub fn registered_origins() -> [&'static dyn OriginHandler; 1] {
    [&PATH_ORIGIN]
}

pub fn get_origin(tag: &str) -> Option<&'static dyn OriginHandler> {
    registered_origins()
        .iter()
        .find(|origin| origin.spec().tag.eq_ignore_ascii_case(tag))
        .copied()
}

pub fn supported_origin_tags() -> Vec<&'static str> {
    registered_origins()
        .iter()
        .map(|origin| origin.spec().tag)
        .collect()
}

pub(crate) fn parse_origin_value(
    value: Value,
    field_path: &str,
) -> Result<Box<dyn ParsedOrigin>, RuntimeError> {
    let object = value
        .as_object()
        .cloned()
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{field_path}: expected object")))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::RecipeLoad(format!("{field_path}.type: expected string")))?;
    let supported = supported_origin_tags().join(", ");
    let handler = get_origin(kind).ok_or_else(|| {
        RuntimeError::RecipeLoad(format!(
            "{field_path}.type: unsupported source origin type '{kind}' (supported: {supported})"
        ))
    })?;
    handler
        .parse(object, field_path)
        .map_err(RuntimeError::RecipeLoad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_origin_is_registered() {
        let origin = get_origin("path").expect("path origin should be registered");
        assert_eq!(origin.spec().tag, "path");
    }
}
