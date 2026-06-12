mod http;
pub mod oci_registry;
mod origins;

use mbuild_core::{ObjectHash, ParsedOrigin};
use serde_json::{Map, Value};
use std::fmt;

/// Error reported while parsing a source recipe node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRecipeError {
    message: String,
}

impl SourceRecipeError {
    /// Creates a source recipe parse error from a user-facing message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the user-facing error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SourceRecipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SourceRecipeError {}

/// Parsed source node prepared for graph planning and source execution.
#[derive(Debug, Clone)]
pub struct SourcePlannedSubject {
    name: String,
    object_hash: ObjectHash,
    origin: Option<Box<dyn ParsedOrigin>>,
}

impl SourcePlannedSubject {
    /// Creates a parsed source subject from its recipe name, declared object
    /// hash, and optional materialization origin.
    pub fn new(
        name: String,
        object_hash: ObjectHash,
        origin: Option<Box<dyn ParsedOrigin>>,
    ) -> Self {
        Self {
            name,
            object_hash,
            origin,
        }
    }

    /// Returns the source recipe name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the source tag.
    pub fn tag(&self) -> &str {
        "Source"
    }

    /// Returns the declared object hash.
    pub fn object_hash(&self) -> ObjectHash {
        self.object_hash
    }

    /// Clones the parsed origin when one was declared.
    pub fn clone_origin(&self) -> Option<Box<dyn ParsedOrigin>> {
        self.origin.clone()
    }
}

/// Parses a raw source recipe object into a planned source subject.
pub fn parse_source_subject(
    mut object: Map<String, Value>,
    path: &str,
) -> Result<SourcePlannedSubject, SourceRecipeError> {
    let name = take_string(&mut object, path, "name")?;
    let tag = take_string(&mut object, path, "tag")?;
    if tag != "Source" {
        return Err(SourceRecipeError::new(format!(
            "{path}.tag: expected 'Source', got '{tag}'"
        )));
    }

    let object_hash = take_string(&mut object, path, "object_hash")?
        .trim()
        .parse::<ObjectHash>()
        .map_err(|error| {
            SourceRecipeError::new(format!("{path}.object_hash: invalid object hash: {error}"))
        })?;
    let origin = match object.remove("origin") {
        Some(value) => Some(origins::parse_origin_value(
            value,
            &format!("{path}.origin"),
        )?),
        None => None,
    };
    if !object.is_empty() {
        return Err(SourceRecipeError::new(format!(
            "{path}: unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    Ok(SourcePlannedSubject::new(name, object_hash, origin))
}

fn take_string(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<String, SourceRecipeError> {
    let value = object.remove(field).ok_or_else(|| {
        SourceRecipeError::new(format!("{path}: missing required field '{field}'"))
    })?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| SourceRecipeError::new(format!("{path}.{field}: expected string")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source_object(origin: Option<Value>) -> Map<String, Value> {
        let mut object = json!({
            "name": "local-source",
            "tag": "Source",
            "object_hash": "1111111111111111111111111111111111111111111111111111111111111111"
        })
        .as_object()
        .cloned()
        .unwrap();
        if let Some(origin) = origin {
            object.insert("origin".to_string(), origin);
        }
        object
    }

    #[test]
    fn source_without_origin_is_accepted() {
        let subject = parse_source_subject(source_object(None), "$.nodes.root").unwrap();

        assert_eq!(subject.name(), "local-source");
        assert_eq!(subject.tag(), "Source");
        assert_eq!(
            subject.object_hash().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert!(subject.clone_origin().is_none());
    }

    #[test]
    fn source_path_origin_is_accepted() {
        let subject = parse_source_subject(
            source_object(Some(json!({
                "tag": "Path",
                "path": "/tmp/source.tar",
                "unpack": true
            }))),
            "$.nodes.root",
        )
        .unwrap();

        assert_eq!(subject.clone_origin().unwrap().spec().tag, "Path");
    }

    #[test]
    fn source_path_origin_requires_absolute_paths() {
        let error = parse_source_subject(
            source_object(Some(json!({
                "tag": "Path",
                "path": "source.tar",
                "unpack": true
            }))),
            "$.nodes.root",
        )
        .unwrap_err();

        assert!(error.to_string().contains("expected absolute path"));
    }

    #[test]
    fn source_http_origin_is_accepted() {
        let subject = parse_source_subject(
            source_object(Some(json!({
                "tag": "Http",
                "url": "https://example.invalid/source.tar.gz",
                "unpack": true
            }))),
            "$.nodes.root",
        )
        .unwrap();

        assert_eq!(subject.clone_origin().unwrap().spec().tag, "Http");
    }

    #[test]
    fn source_oci_registry_origin_is_accepted() {
        let subject = parse_source_subject(
            source_object(Some(json!({
                "tag": "OciRegistry",
                "image": "docker.io/library/alpine:3.20",
                "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }))),
            "$.nodes.root",
        )
        .unwrap();

        assert_eq!(subject.clone_origin().unwrap().spec().tag, "OciRegistry");
    }

    #[test]
    fn source_object_hash_allows_trailing_whitespace() {
        let mut object = source_object(None);
        object.insert(
            "object_hash".to_string(),
            Value::String(
                "1111111111111111111111111111111111111111111111111111111111111111\n".to_string(),
            ),
        );

        let subject = parse_source_subject(object, "$.nodes.root").unwrap();

        assert_eq!(
            subject.object_hash().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn source_tag_must_be_source() {
        let mut object = source_object(None);
        object.insert("tag".to_string(), Value::String("Tree".to_string()));

        let error = parse_source_subject(object, "$.nodes.root").unwrap_err();

        assert!(error.to_string().contains("expected 'Source'"));
    }
}
