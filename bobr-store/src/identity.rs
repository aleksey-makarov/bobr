//! Stable store identity computation.
//!
//! This module computes deterministic identifiers from normalized semantic
//! inputs: build keys for requested invocations, reuse keys for realized input
//! objects, and object hashes for normalized filesystem objects.

use crate::{ReuseInputIdentity, StoreError};
use fsobj_hash::define_hex_hash_type;
pub use fsobj_hash::{ObjectHash, ParseHexHashError};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const INVOCATION_SCHEMA: &str = "bobr-build-invocation-v1";
const REUSE_INVOCATION_SCHEMA: &str = "bobr-build-reuse-invocation-v1";

define_hex_hash_type! {
    /// Stable key for a normalized build invocation.
    ///
    /// A build key is the SHA-256 digest produced by [`compute_build_key`]. It
    /// identifies the builder tag, normalized payload, and input build keys,
    /// independent of whether the corresponding object has already been
    /// published.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct BuildKey;
}

define_hex_hash_type! {
    /// Stable key used to reuse an existing object across equivalent inputs.
    ///
    /// A reuse key is produced by [`compute_reuse_key`] from the builder tag,
    /// normalized payload, and input object identities. It maps to an
    /// [`ObjectHash`] through the store's `reuses` references.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct ReuseKey;
}

/// Computes the stable key for a normalized build invocation.
///
/// The key covers the builder tag, the normalized JSON payload, and the ordered
/// list of input build keys. The payload is serialized with the store's
/// canonical JSON encoder before hashing, so callers must pass the already
/// normalized semantic payload rather than arbitrary user input.
pub fn compute_build_key(
    builder_tag: &str,
    normalized_payload: &Value,
    input_build_keys: &[BuildKey],
) -> Result<BuildKey, StoreError> {
    let input_keys = input_build_keys
        .iter()
        .map(|key| Value::String(key.to_string()))
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
    root.insert("input_build_keys".to_string(), Value::Array(input_keys));

    let canonical = crate::json::canonical_json_bytes(&Value::Object(root))?;
    Ok(BuildKey::from_bytes(Sha256::digest(&canonical).into()))
}

/// Computes the stable reuse key for a normalized object invocation.
///
/// The key covers the builder tag, the normalized JSON payload, and the ordered
/// list of realized input object identities. Runtime code uses this key to find
/// a reusable object even when the current build key has not been seen before.
pub fn compute_reuse_key(
    builder_tag: &str,
    normalized_payload: &Value,
    inputs: &[ReuseInputIdentity],
) -> Result<ReuseKey, StoreError> {
    let input_values = inputs
        .iter()
        .map(|input| {
            let mut object = Map::new();
            object.insert(
                "object_hash".to_string(),
                Value::String(input.object_hash.to_string()),
            );
            Value::Object(object)
        })
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

    let canonical = crate::json::canonical_json_bytes(&Value::Object(root))?;
    Ok(ReuseKey::from_bytes(Sha256::digest(&canonical).into()))
}
