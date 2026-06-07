use crate::fsutil as private_fs;
use crate::identity::{BuildKey, ResultId};
use crate::{Store, StoreError};
use fsobj_hash::ObjectHash;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;

pub(crate) const RESULT_SCHEMA: &str = "bobr-result-v5";
#[cfg(test)]
pub(crate) const BUILD_SCHEMA: &str = RESULT_SCHEMA;

/// Identity of an input result used by reuse-key computation and result records.
///
/// Reuse is based on realized input object identities rather than only on input
/// build keys. This lets equivalent input objects participate in reuse even
/// when they were produced through different build keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseInputIdentity {
    /// Hash of the realized input object.
    pub object_hash: ObjectHash,
}

/// Public build handle resolved from a build key.
///
/// A build handle connects a [`BuildKey`] to the [`ResultId`] and object hash of
/// the realized output. It is the deserializable public view returned when a
/// stored build reference is resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    /// Build invocation key that was requested.
    pub build_key: BuildKey,
    /// Result record id reached by the build reference.
    pub result_id: ResultId,
    /// Hash of the output object recorded by the result.
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    /// Optional RFC 3339 creation timestamp copied from the result record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Store record for a realized result object.
///
/// Result records are stored as canonical JSON under the store's result record
/// directory. The record id is derived from [`ResultRecord::object_hash`], not
/// from the build key that first produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRecord {
    /// Hash of the output object this record describes.
    pub object_hash: ObjectHash,
    /// Optional RFC 3339 timestamp for when the result was created.
    pub created_at: Option<String>,
    /// Realized input object identities used for reuse accounting.
    pub inputs: Vec<ReuseInputIdentity>,
}

impl ResultRecord {
    /// Returns the deterministic id for this result record.
    ///
    /// The id is computed from [`ResultRecord::object_hash`] and therefore does
    /// not depend on the build or reuse key that points to the result.
    pub fn result_id(&self) -> ResultId {
        crate::identity::compute_result_id(self.object_hash)
    }
}

/// Result information returned to runtime code after resolving or publishing.
///
/// This is the serializable representation used when the runtime needs both
/// the object identity and, optionally, the build key that led to the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizedResult {
    /// Result record id.
    pub result_id: ResultId,
    /// Build key that resolved to the result, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_key: Option<BuildKey>,
    /// Hash of the output object.
    #[serde(with = "serde_object_hash")]
    pub object_hash: ObjectHash,
    /// Optional RFC 3339 creation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Fully resolved build publication inside the local store.
///
/// This combines the public build handle, the underlying result record, and the
/// local filesystem path of the imported output object.
#[derive(Debug, Clone)]
pub struct PublishedBuild {
    /// Build handle resolved from the build reference.
    pub build: Build,
    /// Result record reached by the build handle.
    pub result: ResultRecord,
    /// Local path of the imported output object in the store.
    pub object_path: PathBuf,
}

/// Result record resolved to an existing object in the local store.
///
/// A `StoredResult` is stronger than a raw [`ResultRecord`]: loading it checks
/// that the record exists and that its object path exists in the store.
#[derive(Debug, Clone)]
pub struct StoredResult {
    /// Result record loaded from the store.
    pub result: ResultRecord,
    /// Local path of the result object in the store.
    pub object_path: PathBuf,
}

impl StoredResult {
    /// Returns the deterministic id of the stored result.
    pub fn result_id(&self) -> ResultId {
        self.result.result_id()
    }
}

/// Loads a result record by id.
///
/// Returns `Ok(None)` when the record file does not exist. Existing files are
/// parsed as the current canonical result schema and are validated against the
/// requested `result_id`.
pub fn load_result_record(
    store: &Store,
    result_id: ResultId,
) -> Result<Option<ResultRecord>, StoreError> {
    let result_path = store.result_record_path(result_id);
    if !result_path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&result_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read result record '{}': {error}",
            result_path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::InvalidData(format!(
            "failed to parse result record '{}': {error}",
            result_path.display()
        ))
    })?;
    Ok(Some(parse_result_record_value(result_id, &value)?))
}

/// Loads a result record and verifies that its object exists in the store.
///
/// Returns `Ok(None)` when the record file does not exist. Existing records are
/// parsed as canonical result JSON and must point to an existing object.
pub fn load_stored_result(
    store: &Store,
    result_id: ResultId,
) -> Result<Option<StoredResult>, StoreError> {
    let Some(result) = load_result_record(store, result_id)? else {
        return Ok(None);
    };
    Ok(Some(stored_result_from_record(store, result)?))
}

/// Records a source result for an object already present in the store.
///
/// The object path for `object_hash` must exist. The result record is written
/// idempotently and then reloaded as a checked [`StoredResult`].
pub(crate) fn record_existing_source_result(
    store: &Store,
    object_hash: ObjectHash,
    created_at: &str,
) -> Result<StoredResult, StoreError> {
    let object_path = store.object_path(object_hash);
    if !object_path.exists() {
        return Err(StoreError::Io(format!(
            "source object '{}' is missing from store at '{}'",
            object_hash,
            object_path.display()
        )));
    }

    let result = ResultRecord {
        object_hash,
        created_at: Some(created_at.to_string()),
        inputs: Vec::new(),
    };
    store_result_record(store, &result)?;
    load_stored_result(store, result.result_id())?.ok_or_else(|| {
        StoreError::InvalidData(format!(
            "source result '{}' was not stored",
            result.result_id()
        ))
    })
}

pub(crate) fn stored_result_from_record(
    store: &Store,
    result: ResultRecord,
) -> Result<StoredResult, StoreError> {
    let result_id = result.result_id();
    let object_path = store.object_path(result.object_hash);
    if !object_path.exists() {
        return Err(StoreError::Io(format!(
            "result '{}' points to missing object '{}'",
            result_id,
            object_path.display()
        )));
    }
    Ok(StoredResult {
        result,
        object_path,
    })
}

/// Stores a result record if it is not already present.
///
/// The record is written as canonical JSON under the store's result record
/// directory. The operation is idempotent for an already-existing record path.
pub(crate) fn store_result_record(store: &Store, record: &ResultRecord) -> Result<(), StoreError> {
    let result_path = store.result_record_path(record.result_id());
    if result_path.exists() {
        return Ok(());
    }
    let result_value = result_record_json_value(record);
    let canonical = crate::json::canonical_json_bytes(&result_value)?;
    private_fs::write_atomic(
        &result_path,
        std::str::from_utf8(&canonical).map_err(|error| {
            StoreError::InvalidData(format!(
                "failed to encode canonical result JSON as UTF-8: {error}"
            ))
        })?,
    )
    .map_err(crate::error::map_fsutil_error)
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
) -> Result<ResultRecord, StoreError> {
    let object = value.as_object().ok_or_else(|| {
        StoreError::InvalidData("result record root must be a JSON object".to_string())
    })?;

    let schema = object
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::InvalidData("result record is missing 'schema'".to_string()))?;
    if schema != RESULT_SCHEMA {
        return Err(StoreError::InvalidData(format!(
            "unsupported result record schema '{schema}'"
        )));
    }

    let created_at = object
        .get("created_at")
        .map(|value| {
            value.as_str().ok_or_else(|| {
                StoreError::InvalidData("result record created_at must be a string".to_string())
            })
        })
        .transpose()?
        .map(str::to_string);

    let object_hash = object
        .get("object_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            StoreError::InvalidData("result record is missing 'object_hash'".to_string())
        })
        .and_then(parse_object_hash_result)?;

    let computed_result_id = crate::identity::compute_result_id(object_hash);
    if computed_result_id != result_id {
        return Err(StoreError::InvalidData(format!(
            "result record key mismatch: path key '{}' does not match object hash '{}' computed key '{}'",
            result_id, object_hash, computed_result_id
        )));
    }

    let inputs = object
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::InvalidData("result record is missing 'inputs'".to_string()))?
        .iter()
        .map(|value| {
            let object = value.as_object().ok_or_else(|| {
                StoreError::InvalidData("result record inputs must contain objects".to_string())
            })?;
            let object_hash = object
                .get("object_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidData(
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

fn parse_object_hash_result(value: &str) -> Result<ObjectHash, StoreError> {
    value.parse::<ObjectHash>().map_err(|error| {
        StoreError::InvalidData(format!(
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
