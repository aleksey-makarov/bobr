//! Source materialization: fetch and stage source objects from their origins.
//!
//! A *source* is a leaf build input — it has no inputs of its own, so its
//! content is fixed up front rather than computed by a build. That is exactly
//! what lets a source be *pinned* by an object hash: with nothing to depend on,
//! the content is known in advance, and the hash both names it and verifies it
//! after fetching. An *origin* only describes where to obtain those bytes.
//!
//! An [`OriginHandler`] parses a recipe's `origin` object into a
//! [`ParsedOrigin`], which materializes the content into a staging directory the
//! runtime owns and cleans up; the result is then checked against the declared
//! hash.
//!
//! The core abstractions — [`OriginSpec`], [`OriginContext`], [`OriginHandler`],
//! and [`ParsedOrigin`] — are exposed at the crate root.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

pub mod acquisition;
pub mod build_executor;
pub mod dynamic_realizer;
pub mod graph;
mod http;
/// OCI registry client: pulls and stages an image's layers by pinned digest.
///
/// Public only under the `test-support` feature (used by `bobr`'s integration
/// tests); crate-private otherwise.
#[cfg(feature = "test-support")]
pub mod oci_registry;
#[cfg(not(feature = "test-support"))]
mod oci_registry;
mod origin;
mod origins;
pub mod realizer;

// The origin abstractions are the crate's public API; re-export them at the root
// rather than exposing the module path.
pub use origin::{OriginContext, OriginHandler, OriginSpec, ParsedOrigin};

use bobr_core::{BuildKey, BuildLogSubject, ObjectHash, Workspace};
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

/// Parsed Source node prepared for graph planning and async acquisition.
#[derive(Debug, Clone)]
pub struct SourcePlannedSubject {
    name: String,
    build_key: BuildKey,
    declared_object_hash: ObjectHash,
    origin_value: Option<Value>,
}

impl SourcePlannedSubject {
    /// Creates a planned Source after request parsing validated its origin.
    pub(crate) fn new(
        name: String,
        declared_object_hash: ObjectHash,
        origin: Option<Value>,
    ) -> Self {
        Self {
            name,
            build_key: BuildKey::from_object_hash(declared_object_hash),
            declared_object_hash,
            origin_value: origin,
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

    /// Returns the source build key.
    pub fn build_key(&self) -> BuildKey {
        self.build_key
    }

    /// Returns the declared object hash.
    pub fn declared_object_hash(&self) -> ObjectHash {
        self.declared_object_hash
    }

    /// Returns the validated raw origin object for async acquisition.
    pub fn origin_value(&self) -> Option<&Value> {
        self.origin_value.as_ref()
    }

    /// Builds the per-run log subject from the runtime-allocated workspace.
    pub fn log_subject(&self, workspace: &Workspace) -> BuildLogSubject {
        BuildLogSubject::new(
            self.tag(),
            self.name(),
            self.build_key().to_string(),
            workspace.log_dir().to_path_buf(),
            workspace.raw_log_dir().to_path_buf(),
        )
    }
}

/// Parses a source recipe object whose tag was already removed by the caller.
pub fn parse_source_subject(
    mut object: Map<String, Value>,
) -> Result<SourcePlannedSubject, SourceRecipeError> {
    let name = take_string(&mut object, "name")?;
    let declared_object_hash = take_string(&mut object, "object_hash")?
        .trim()
        .parse::<ObjectHash>()
        .map_err(|error| {
            SourceRecipeError::new(format!("object_hash: invalid object hash: {error}"))
        })?;
    let origin_value = object.remove("origin");
    if let Some(value) = origin_value.clone() {
        let _ = origins::parse_origin_value(value, "origin")?;
    }
    if !object.is_empty() {
        return Err(SourceRecipeError::new(format!(
            "unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    Ok(SourcePlannedSubject::new(
        name,
        declared_object_hash,
        origin_value,
    ))
}

fn take_string(object: &mut Map<String, Value>, field: &str) -> Result<String, SourceRecipeError> {
    let value = object
        .remove(field)
        .ok_or_else(|| SourceRecipeError::new(format!("missing required field '{field}'")))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| SourceRecipeError::new(format!("{field}: expected string")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source_object(origin: Option<Value>) -> Map<String, Value> {
        let mut object = json!({
            "name": "local-source",
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
        let subject = parse_source_subject(source_object(None)).unwrap();

        assert_eq!(subject.name(), "local-source");
        assert_eq!(subject.tag(), "Source");
        assert_eq!(
            subject.declared_object_hash().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(
            subject.build_key().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert!(subject.origin_value().is_none());
    }

    #[test]
    fn source_path_origin_is_accepted() {
        let subject = parse_source_subject(source_object(Some(json!({
            "tag": "Path",
            "path": "/tmp/source.tar",
            "unpack": true
        }))))
        .unwrap();

        assert_eq!(subject.origin_value().unwrap()["tag"], "Path");
    }

    #[test]
    fn source_path_origin_requires_absolute_paths() {
        let error = parse_source_subject(source_object(Some(json!({
            "tag": "Path",
            "path": "source.tar",
            "unpack": true
        }))))
        .unwrap_err();

        assert!(error.to_string().contains("expected absolute path"));
    }

    #[test]
    fn source_http_origin_is_accepted() {
        let subject = parse_source_subject(source_object(Some(json!({
            "tag": "Http",
            "url": "https://example.invalid/source.tar.gz",
            "unpack": true
        }))))
        .unwrap();

        assert_eq!(subject.origin_value().unwrap()["tag"], "Http");
    }

    #[test]
    fn source_oci_registry_origin_is_accepted() {
        let subject = parse_source_subject(source_object(Some(json!({
            "tag": "OciRegistry",
            "image": "docker.io/library/alpine:3.20",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "platform": {
                "os": "linux",
                "architecture": "amd64"
            }
        }))))
        .unwrap();

        assert_eq!(subject.origin_value().unwrap()["tag"], "OciRegistry");
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

        let subject = parse_source_subject(object).unwrap();

        assert_eq!(
            subject.declared_object_hash().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
    }
}
