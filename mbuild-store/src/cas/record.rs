use super::{BuildKey, CasError, ResultId, StoreLayout};
use crate::fsutil;
use fsobj_hash::ObjectHash;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;

pub(crate) const RESULT_SCHEMA: &str = "mbuild-result-v5";
#[cfg(test)]
pub(crate) const BUILD_SCHEMA: &str = RESULT_SCHEMA;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseInputIdentity {
    pub object_hash: ObjectHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub build_key: BuildKey,
    pub result_id: ResultId,
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRecord {
    pub object_hash: ObjectHash,
    pub created_at: Option<String>,
    pub inputs: Vec<ReuseInputIdentity>,
}

impl ResultRecord {
    pub fn result_id(&self) -> ResultId {
        super::key::result_id_for_object_hash(self.object_hash)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizedResult {
    pub result_id: ResultId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_key: Option<BuildKey>,
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishedBuild {
    pub build: Build,
    pub result: ResultRecord,
    pub object_path: PathBuf,
}

pub fn load_result_record(
    layout: &StoreLayout,
    result_id: ResultId,
) -> Result<Option<ResultRecord>, CasError> {
    let result_path = super::refs::result_path(layout, result_id);
    if !result_path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&result_path).map_err(|error| {
        CasError::Io(format!(
            "failed to read result record '{}': {error}",
            result_path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        CasError::Serialization(format!(
            "failed to parse result record '{}': {error}",
            result_path.display()
        ))
    })?;
    Ok(Some(parse_result_record_value(result_id, &value)?))
}

pub fn store_result_record(layout: &StoreLayout, record: &ResultRecord) -> Result<(), CasError> {
    let result_path = super::refs::result_path(layout, record.result_id());
    if result_path.exists() {
        return Ok(());
    }
    let result_value = result_record_json_value(record);
    let canonical = super::json::canonical_json_bytes(&result_value)?;
    fsutil::write_atomic(
        &result_path,
        std::str::from_utf8(&canonical).map_err(|error| {
            CasError::Serialization(format!(
                "failed to encode canonical result JSON as UTF-8: {error}"
            ))
        })?,
    )
    .map_err(super::error::map_fsutil_error)
}

pub(crate) fn build_from_result(build_key: BuildKey, result: &ResultRecord) -> Build {
    Build {
        build_key,
        result_id: result.result_id(),
        object_hash: result.object_hash,
        created_at: result.created_at.clone(),
    }
}

fn result_json_value(
    created_at: Option<&str>,
    object_hash: ObjectHash,
    inputs: &[ReuseInputIdentity],
) -> Value {
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
        Value::String(RESULT_SCHEMA.to_string()),
    );
    if let Some(created_at) = created_at {
        root.insert(
            "created_at".to_string(),
            Value::String(created_at.to_string()),
        );
    }
    root.insert(
        "object_hash".to_string(),
        Value::String(object_hash.to_string()),
    );
    root.insert("inputs".to_string(), Value::Array(input_values));
    Value::Object(root)
}

fn result_record_json_value(record: &ResultRecord) -> Value {
    result_json_value(
        record.created_at.as_deref(),
        record.object_hash,
        &record.inputs,
    )
}

#[cfg(test)]
pub(crate) fn build_json_value(
    created_at: Option<&str>,
    object_hash: ObjectHash,
    inputs: &[ReuseInputIdentity],
) -> Value {
    result_json_value(created_at, object_hash, inputs)
}

pub(crate) fn parse_result_record_value(
    result_id: ResultId,
    value: &Value,
) -> Result<ResultRecord, CasError> {
    let object = value.as_object().ok_or_else(|| {
        CasError::Serialization("result record root must be a JSON object".to_string())
    })?;

    let schema = object
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("result record is missing 'schema'".to_string()))?;
    if schema != RESULT_SCHEMA {
        return Err(CasError::Serialization(format!(
            "unsupported result record schema '{schema}'"
        )));
    }

    let created_at = object
        .get("created_at")
        .map(|value| {
            value.as_str().ok_or_else(|| {
                CasError::Serialization("result record created_at must be a string".to_string())
            })
        })
        .transpose()?
        .map(str::to_string);

    let object_hash = object
        .get("object_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CasError::Serialization("result record is missing 'object_hash'".to_string())
        })
        .and_then(parse_object_hash_result)?;

    let computed_result_id = super::key::compute_result_id(object_hash)?;
    if computed_result_id != result_id {
        return Err(CasError::Serialization(format!(
            "result record key mismatch: path key '{}' does not match object hash '{}' computed key '{}'",
            result_id, object_hash, computed_result_id
        )));
    }

    let inputs = object
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| CasError::Serialization("result record is missing 'inputs'".to_string()))?
        .iter()
        .map(|value| {
            let object = value.as_object().ok_or_else(|| {
                CasError::Serialization("result record inputs must contain objects".to_string())
            })?;
            let object_hash = object
                .get("object_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CasError::Serialization(
                        "result record input is missing 'object_hash'".to_string(),
                    )
                })
                .and_then(parse_object_hash_result)?;
            Ok(ReuseInputIdentity { object_hash })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ResultRecord {
        object_hash,
        created_at,
        inputs,
    })
}

fn parse_object_hash_result(value: &str) -> Result<ObjectHash, CasError> {
    value.parse::<ObjectHash>().map_err(|error| {
        CasError::Serialization(format!(
            "invalid object hash '{value}' in result record: {error}"
        ))
    })
}

mod serde_object_hash {
    use fsobj_hash::ObjectHash;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};
    use std::str::FromStr;

    pub fn serialize<S>(value: &ObjectHash, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ObjectHash, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        ObjectHash::from_str(&value).map_err(D::Error::custom)
    }
}
