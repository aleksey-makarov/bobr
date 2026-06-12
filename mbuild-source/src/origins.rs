mod path;

use mbuild_core::{OriginHandler, ParsedOrigin};
use serde_json::Value;

use self::path::PathOriginHandler;
use crate::SourceRecipeError;
use crate::http::HttpOriginHandler;
use crate::oci_registry::OciRegistryOriginHandler;

static PATH_ORIGIN: PathOriginHandler = PathOriginHandler;
static HTTP_ORIGIN: HttpOriginHandler = HttpOriginHandler;
static OCI_REGISTRY_ORIGIN: OciRegistryOriginHandler = OciRegistryOriginHandler;

fn registered_origins() -> [&'static dyn OriginHandler; 3] {
    [&PATH_ORIGIN, &HTTP_ORIGIN, &OCI_REGISTRY_ORIGIN]
}

fn get_origin(tag: &str) -> Option<&'static dyn OriginHandler> {
    registered_origins()
        .iter()
        .find(|origin| origin.spec().tag == tag)
        .copied()
}

fn supported_origin_tags() -> Vec<&'static str> {
    registered_origins()
        .iter()
        .map(|origin| origin.spec().tag)
        .collect()
}

pub(crate) fn parse_origin_value(
    value: Value,
    field_path: &str,
) -> Result<Box<dyn ParsedOrigin>, SourceRecipeError> {
    let object = value
        .as_object()
        .cloned()
        .ok_or_else(|| SourceRecipeError::new(format!("{field_path}: expected object")))?;
    let tag = object
        .get("tag")
        .and_then(Value::as_str)
        .ok_or_else(|| SourceRecipeError::new(format!("{field_path}.tag: expected string")))?;
    let supported = supported_origin_tags().join(", ");
    let handler = get_origin(tag).ok_or_else(|| {
        SourceRecipeError::new(format!(
            "{field_path}.tag: unsupported source origin tag '{tag}' (supported: {supported})"
        ))
    })?;
    handler
        .parse(object, field_path)
        .map_err(SourceRecipeError::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_origin_is_registered() {
        let origin = get_origin("Path").expect("Path origin should be registered");
        assert_eq!(origin.spec().tag, "Path");
    }

    #[test]
    fn http_origin_is_registered() {
        let origin = get_origin("Http").expect("Http origin should be registered");
        assert_eq!(origin.spec().tag, "Http");
    }

    #[test]
    fn oci_registry_origin_is_registered() {
        let origin = get_origin("OciRegistry").expect("OciRegistry origin should be registered");
        assert_eq!(origin.spec().tag, "OciRegistry");
    }
}
