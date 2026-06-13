//! Stable store identity computation.
//!
//! This module computes deterministic identifiers from normalized semantic
//! inputs: requested invocation keys, reuse keys for realized input objects,
//! and object hashes for normalized filesystem objects.

use crate::{ReuseInputIdentity, StoreError};
use fsobj_hash::define_hex_hash_type;
pub use fsobj_hash::{ObjectHash, ParseHexHashError};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const INVOCATION_SCHEMA: &str = "bobr-build-invocation-v2";
const REUSE_INVOCATION_SCHEMA: &str = "bobr-build-reuse-invocation-v1";

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
/// list of direct dependency build keys. The payload is serialized with the
/// store's canonical JSON encoder before hashing, so callers must pass the
/// already normalized semantic payload rather than arbitrary user input.
pub fn compute_build_key(
    builder_tag: &str,
    normalized_payload: &Value,
    input_keys: &[BuildKey],
) -> Result<BuildKey, StoreError> {
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
