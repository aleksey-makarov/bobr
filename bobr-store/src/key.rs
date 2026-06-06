use crate::{BuildKey, ResultId, ReuseInputIdentity, ReuseKey, StoreError};
use fsobj_hash::ObjectHash;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const INVOCATION_SCHEMA: &str = "bobr-build-invocation-v1";
const RESULT_INVOCATION_SCHEMA: &str = "bobr-build-result-invocation-v3";

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

/// Computes the stable reuse key for a normalized result invocation.
///
/// The key covers the builder tag, the normalized JSON payload, and the ordered
/// list of realized input object identities. Runtime code uses this key to find
/// a reusable result even when the current build key has not been seen before.
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
        Value::String(RESULT_INVOCATION_SCHEMA.to_string()),
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

/// Computes the stable result id for an imported object hash.
///
/// The result id is the key under which the result record is stored. It is
/// derived only from the output object's [`ObjectHash`], so equivalent output
/// objects share the same result record.
pub fn compute_result_id(object_hash: ObjectHash) -> Result<ResultId, StoreError> {
    Ok(result_id_for_object_hash(object_hash))
}

pub(crate) fn result_id_for_object_hash(object_hash: ObjectHash) -> ResultId {
    let canonical = format!(
        "{{\"object_hash\":\"{}\",\"schema\":\"bobr-result-id-v2\"}}",
        object_hash
    );
    ResultId::from_bytes(Sha256::digest(canonical.as_bytes()).into())
}
