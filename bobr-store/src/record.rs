use crate::fsutil as private_fs;
use crate::{Store, StoreError};
use bobr_core::{BuildKey, ObjectHash};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;

pub(crate) const OBJECT_RECORD_SCHEMA: &str = "bobr-object-record-v3";
#[cfg(test)]
pub(crate) const OBJECT_RECORD_SCHEMA_FOR_TEST: &str = OBJECT_RECORD_SCHEMA;

/// Public build handle resolved from a build key.
///
/// A build handle connects a [`BuildKey`] to the object hash of the realized
/// output. It is the deserializable public view returned when a stored build
/// reference is resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    /// Build invocation key that was requested.
    pub build_key: BuildKey,
    /// Hash of the output object recorded by the object record.
    pub object_hash: ObjectHash,
    /// Optional store run id copied from the object record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Store record for a realized object.
///
/// Object records are stored as canonical JSON under the store's object record
/// directory and are keyed by [`ObjectRecord::object_hash`], not by the build
/// key that first produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRecord {
    /// Hash of the output object this record describes.
    pub object_hash: ObjectHash,
    /// Optional store run id that recorded this object.
    pub run_id: Option<String>,
    /// Realized input object hashes used for reuse accounting.
    pub inputs: Vec<ObjectHash>,
}

/// Object information returned to runtime code after resolving or publishing.
///
/// This is the serializable representation used when the runtime needs both
/// the object identity and, optionally, the build key that led to the object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizedObject {
    /// Build key that resolved to the object, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_key: Option<BuildKey>,
    /// Hash of the output object.
    pub object_hash: ObjectHash,
    /// Optional store run id that recorded this object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Fully resolved build publication inside the local store.
///
/// This combines the public build handle, the underlying object record, and the
/// local filesystem path of the imported output object.
#[derive(Debug, Clone)]
pub struct PublishedBuild {
    /// Build handle resolved from the build reference.
    pub build: Build,
    /// Object record reached by the build handle.
    pub object_record: ObjectRecord,
    /// Local path of the imported output object in the store.
    pub object_path: PathBuf,
}

/// Object record resolved to an existing object in the local store.
///
/// A `StoredObjectRecord` is stronger than a raw [`ObjectRecord`]: loading it checks
/// that the record exists and that its object path exists in the store.
#[derive(Debug, Clone)]
pub struct StoredObjectRecord {
    /// Object record loaded from the store.
    pub object_record: ObjectRecord,
    /// Local path of the recorded object in the store.
    pub object_path: PathBuf,
}

/// Loads an object record by object hash.
///
/// Returns `Ok(None)` when the record file does not exist. Existing files are
/// parsed as the current canonical object record schema and are validated against
/// the requested `object_hash`.
pub fn load_object_record(
    store: &Store,
    object_hash: ObjectHash,
) -> Result<Option<ObjectRecord>, StoreError> {
    let object_record_path = store.object_record_path(object_hash);
    if !object_record_path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&object_record_path).map_err(|error| {
        StoreError::Io(format!(
            "failed to read object record '{}': {error}",
            object_record_path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::InvalidData(format!(
            "failed to parse object record '{}': {error}",
            object_record_path.display()
        ))
    })?;
    Ok(Some(parse_object_record_value(object_hash, &value)?))
}

/// Loads an object record and verifies that its object exists in the store.
///
/// Returns `Ok(None)` when the record file does not exist. Existing records are
/// parsed as canonical object record JSON and must point to an existing object.
pub fn load_stored_object_record(
    store: &Store,
    object_hash: ObjectHash,
) -> Result<Option<StoredObjectRecord>, StoreError> {
    let Some(object_record) = load_object_record(store, object_hash)? else {
        return Ok(None);
    };
    Ok(Some(stored_object_record_from_record(
        store,
        object_record,
    )?))
}

/// Records a source object already present in the store.
///
/// The object path for `object_hash` must exist. The object record is written
/// idempotently and then reloaded as a checked [`StoredObjectRecord`].
pub(crate) fn record_existing_source_object(
    store: &Store,
    object_hash: ObjectHash,
) -> Result<StoredObjectRecord, StoreError> {
    let object_path = store.object_path(object_hash);
    if !object_path.exists() {
        return Err(StoreError::Io(format!(
            "source object '{}' is missing from store at '{}'",
            object_hash,
            object_path.display()
        )));
    }

    let object_record = ObjectRecord {
        object_hash,
        run_id: Some(store.run_id().to_string()),
        inputs: Vec::new(),
    };
    store_object_record(store, &object_record)?;
    load_stored_object_record(store, object_record.object_hash)?.ok_or_else(|| {
        StoreError::InvalidData(format!(
            "source object record for object '{}' was not stored",
            object_record.object_hash
        ))
    })
}

pub(crate) fn stored_object_record_from_record(
    store: &Store,
    object_record: ObjectRecord,
) -> Result<StoredObjectRecord, StoreError> {
    let object_path = store.object_path(object_record.object_hash);
    if !object_path.exists() {
        return Err(StoreError::Io(format!(
            "object record for object '{}' points to missing object '{}'",
            object_record.object_hash,
            object_path.display()
        )));
    }
    Ok(StoredObjectRecord {
        object_record,
        object_path,
    })
}

/// Stores an object record if it is not already present.
///
/// The record is written as canonical JSON under the store's object record
/// directory. The operation is idempotent for an already-existing record path.
pub(crate) fn store_object_record(store: &Store, record: &ObjectRecord) -> Result<(), StoreError> {
    let object_record_path = store.object_record_path(record.object_hash);
    if object_record_path.exists() {
        return Ok(());
    }
    let object_record_value = object_record_json_value(record);
    let canonical = crate::json::canonical_json_bytes(&object_record_value)?;
    private_fs::write_atomic(
        &object_record_path,
        std::str::from_utf8(&canonical).map_err(|error| {
            StoreError::InvalidData(format!(
                "failed to encode canonical object record JSON as UTF-8: {error}"
            ))
        })?,
    )
    .map_err(crate::error::map_fsutil_error)
}

pub(crate) fn build_from_object_record(build_key: BuildKey, object_record: &ObjectRecord) -> Build {
    Build {
        build_key,
        object_hash: object_record.object_hash,
        run_id: object_record.run_id.clone(),
    }
}

fn object_record_json_value_from_parts(
    run_id: Option<&str>,
    object_hash: ObjectHash,
    inputs: &[ObjectHash],
) -> Value {
    let input_values = inputs
        .iter()
        .map(|input_hash| Value::String(input_hash.to_string()))
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(OBJECT_RECORD_SCHEMA.to_string()),
    );
    if let Some(run_id) = run_id {
        root.insert("run_id".to_string(), Value::String(run_id.to_string()));
    }
    root.insert(
        "object_hash".to_string(),
        Value::String(object_hash.to_string()),
    );
    root.insert("inputs".to_string(), Value::Array(input_values));
    Value::Object(root)
}

fn object_record_json_value(record: &ObjectRecord) -> Value {
    object_record_json_value_from_parts(
        record.run_id.as_deref(),
        record.object_hash,
        &record.inputs,
    )
}

#[cfg(test)]
pub(crate) fn build_json_value(
    run_id: Option<&str>,
    object_hash: ObjectHash,
    inputs: &[ObjectHash],
) -> Value {
    object_record_json_value_from_parts(run_id, object_hash, inputs)
}

pub(crate) fn parse_object_record_value(
    expected_object_hash: ObjectHash,
    value: &Value,
) -> Result<ObjectRecord, StoreError> {
    let object = value.as_object().ok_or_else(|| {
        StoreError::InvalidData("object record root must be a JSON object".to_string())
    })?;

    let schema = object
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::InvalidData("object record is missing 'schema'".to_string()))?;
    if schema != OBJECT_RECORD_SCHEMA {
        return Err(StoreError::InvalidData(format!(
            "unsupported object record schema '{schema}'"
        )));
    }

    let run_id = object
        .get("run_id")
        .map(|value| {
            value.as_str().ok_or_else(|| {
                StoreError::InvalidData("object record run_id must be a string".to_string())
            })
        })
        .transpose()?
        .map(str::to_string);

    let object_hash = object
        .get("object_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            StoreError::InvalidData("object record is missing 'object_hash'".to_string())
        })
        .and_then(parse_object_hash_for_record)?;

    if object_hash != expected_object_hash {
        return Err(StoreError::InvalidData(format!(
            "object record key mismatch: path key '{}' does not match record object hash '{}'",
            expected_object_hash, object_hash
        )));
    }

    let inputs = object
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::InvalidData("object record is missing 'inputs'".to_string()))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| {
                    StoreError::InvalidData(
                        "object record inputs must contain object hash strings".to_string(),
                    )
                })
                .and_then(parse_object_hash_for_record)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ObjectRecord {
        object_hash,
        run_id,
        inputs,
    })
}

fn parse_object_hash_for_record(value: &str) -> Result<ObjectHash, StoreError> {
    value.parse::<ObjectHash>().map_err(|error| {
        StoreError::InvalidData(format!(
            "invalid object hash '{value}' in object record: {error}"
        ))
    })
}
