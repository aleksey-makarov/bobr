//! Stable build identity computation.
//!
//! This module computes deterministic identifiers from normalized semantic
//! inputs: requested build graph keys, reuse keys for realized input objects,
//! and object hashes for normalized filesystem objects.

use fsobj_hash::define_hex_hash_type;
pub use fsobj_hash::{ObjectHash, ParseHexHashError};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;

const INVOCATION_SCHEMA: &str = "bobr-build-invocation-v2";
const REUSE_INVOCATION_SCHEMA: &str = "bobr-build-reuse-invocation-v2";

define_hex_hash_type! {
    /// Stable key for a planned build graph node.
    ///
    /// Builder build keys are SHA-256 digests produced by
    /// [`compute_build_key`]. Source build keys are produced by
    /// [`BuildKey::from_object_hash`] and have the same textual representation
    /// as the declared source object hash.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct BuildKey;
}

impl BuildKey {
    /// Returns the source build key for an object hash.
    ///
    /// Source nodes have no planned inputs. Their graph/build key is the
    /// declared object hash reinterpreted in the build-key domain, so the
    /// textual representation is unchanged.
    pub fn from_object_hash(object_hash: ObjectHash) -> Self {
        Self::from_bytes(*object_hash.as_bytes())
    }

    /// Returns a short human-readable prefix of the key.
    pub fn short(self) -> String {
        self.to_string().chars().take(12).collect()
    }
}

define_hex_hash_type! {
    /// Stable key used to reuse an existing object across equivalent inputs.
    ///
    /// A reuse key is produced by [`compute_reuse_key`] from the builder tag,
    /// normalized payload, and realized input object hashes.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct ReuseKey;
}

/// Error returned while computing build identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityError {
    message: String,
}

impl IdentityError {
    fn json(error: serde_json::Error) -> Self {
        Self {
            message: format!("failed to serialize canonical identity JSON: {error}"),
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IdentityError {}

/// Computes the stable key for a normalized build invocation.
///
/// The key covers the builder tag, the normalized JSON payload, and the ordered
/// list of direct dependency build keys. The payload is serialized with the
/// core canonical JSON encoder before hashing, so callers must pass the already
/// normalized semantic payload rather than arbitrary user input.
pub fn compute_build_key(
    builder_tag: &str,
    normalized_payload: &Value,
    input_keys: &[BuildKey],
) -> Result<BuildKey, IdentityError> {
    let input_values = input_keys
        .iter()
        .map(|build_key| {
            let mut input = Map::new();
            input.insert(
                "build_key".to_string(),
                Value::String(build_key.to_string()),
            );
            Value::Object(input)
        })
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(INVOCATION_SCHEMA.to_string()),
    );
    root.insert(
        "builder_tag".to_string(),
        Value::String(builder_tag.to_string()),
    );
    root.insert("payload".to_string(), normalized_payload.clone());
    root.insert("inputs".to_string(), Value::Array(input_values));

    let canonical = canonical_json_bytes(&Value::Object(root))?;
    Ok(BuildKey::from_bytes(Sha256::digest(&canonical).into()))
}

/// Computes the stable reuse key for a normalized object invocation.
///
/// The key covers the builder tag, the normalized JSON payload, and the ordered
/// list of realized input object hashes. Runtime code uses this key to find
/// a reusable object even when the current build key has not been seen before.
pub fn compute_reuse_key(
    builder_tag: &str,
    normalized_payload: &Value,
    inputs: &[ObjectHash],
) -> Result<ReuseKey, IdentityError> {
    let input_values = inputs
        .iter()
        .map(|object_hash| Value::String(object_hash.to_string()))
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(REUSE_INVOCATION_SCHEMA.to_string()),
    );
    root.insert(
        "builder_tag".to_string(),
        Value::String(builder_tag.to_string()),
    );
    root.insert("payload".to_string(), normalized_payload.clone());
    root.insert("inputs".to_string(), Value::Array(input_values));

    let canonical = canonical_json_bytes(&Value::Object(root))?;
    Ok(ReuseKey::from_bytes(Sha256::digest(&canonical).into()))
}

fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, IdentityError> {
    let mut out = Vec::new();
    write_canonical_json(value, &mut out)?;
    Ok(out)
}

fn write_canonical_json(value: &Value, out: &mut Vec<u8>) -> Result<(), IdentityError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_writer(out, value).map_err(IdentityError::json)
        }
        Value::Array(items) => {
            out.push(b'[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                write_canonical_json(item, out)?;
            }
            out.push(b']');
            Ok(())
        }
        Value::Object(object) => {
            out.push(b'{');
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            for (idx, key) in keys.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).map_err(IdentityError::json)?;
                out.push(b':');
                write_canonical_json(&object[*key], out)?;
            }
            out.push(b'}');
            Ok(())
        }
    }
}
