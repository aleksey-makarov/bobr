use super::{BuildKey, CasError, ResultId, ReuseInputIdentity, ReuseKey};
use fsobj_hash::ObjectHash;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const INVOCATION_SCHEMA: &str = "mbuild-build-invocation-v1";
const RESULT_INVOCATION_SCHEMA: &str = "mbuild-build-result-invocation-v3";

pub fn compute_build_key(
    builder_tag: &str,
    normalized_payload: &Value,
    input_build_keys: &[BuildKey],
) -> Result<BuildKey, CasError> {
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

    let canonical = super::json::canonical_json_bytes(&Value::Object(root))?;
    Ok(BuildKey::from_bytes(Sha256::digest(&canonical).into()))
}

pub fn compute_reuse_key(
    builder_tag: &str,
    normalized_payload: &Value,
    inputs: &[ReuseInputIdentity],
) -> Result<ReuseKey, CasError> {
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

    let canonical = super::json::canonical_json_bytes(&Value::Object(root))?;
    Ok(ReuseKey::from_bytes(Sha256::digest(&canonical).into()))
}

pub fn compute_result_id(object_hash: ObjectHash) -> Result<ResultId, CasError> {
    Ok(result_id_for_object_hash(object_hash))
}

pub(crate) fn result_id_for_object_hash(object_hash: ObjectHash) -> ResultId {
    let canonical = format!(
        "{{\"object_hash\":\"{}\",\"schema\":\"mbuild-result-id-v2\"}}",
        object_hash
    );
    ResultId::from_bytes(Sha256::digest(canonical.as_bytes()).into())
}
