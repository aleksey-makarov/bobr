use crate::fsutil as private_fs;
use crate::{Store, StoreError};
use bobr_core::{BuildKey, ObjectHash};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;
use std::fs;

pub(crate) const OBJECT_RECORD_SCHEMA: &str = "bobr-object-record-v4";

/// Schema marker for the object record format. It (de)serializes only as the
/// current schema string, so the version is enforced declaratively and never
/// needs to live as data on `ObjectRecord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectRecordSchemaV4;

impl Serialize for ObjectRecordSchemaV4 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(OBJECT_RECORD_SCHEMA)
    }
}

impl<'de> Deserialize<'de> for ObjectRecordSchemaV4 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            OBJECT_RECORD_SCHEMA => Ok(ObjectRecordSchemaV4),
            other => Err(D::Error::custom(format!(
                "unsupported object record schema '{other}'"
            ))),
        }
    }
}

/// Store record for a realized object.
///
/// Object records are stored as JSON under the store's object record directory
/// and are keyed by [`ObjectRecord::object_hash`], not by the build key that
/// first produced them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRecord {
    /// Schema marker; enforces the record format version at (de)serialization.
    pub(crate) schema: ObjectRecordSchemaV4,
    /// Build key that first materialized this object.
    pub build_key: BuildKey,
    /// Hash of the output object this record describes.
    pub object_hash: ObjectHash,
    /// Optional store run id that recorded this object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Realized input object hashes used for reuse accounting.
    pub inputs: Vec<ObjectHash>,
}

/// Loads an object record by object hash.
///
/// Returns `Ok(None)` when the record file does not exist. Existing files are
/// parsed as the current object record schema and are validated against the
/// requested `object_hash`.
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

/// Records a source object already present in the store.
///
/// The object for `object_hash` must exist; its object record is written
/// idempotently.
pub(crate) fn record_existing_source_object(
    store: &Store,
    object_hash: ObjectHash,
) -> Result<(), StoreError> {
    if store.object_path(object_hash)?.is_none() {
        return Err(StoreError::Io(format!(
            "source object '{object_hash}' is missing from store"
        )));
    }

    let object_record = ObjectRecord {
        schema: ObjectRecordSchemaV4,
        build_key: BuildKey::from_object_hash(object_hash),
        object_hash,
        run_id: Some(store.run_id().to_string()),
        inputs: Vec::new(),
    };
    store_object_record(store, &object_record)
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
    let json = serde_json::to_string(record).map_err(|error| {
        StoreError::InvalidData(format!("failed to encode object record JSON: {error}"))
    })?;
    private_fs::write_atomic(&object_record_path, &json).map_err(crate::error::map_fsutil_error)
}

pub(crate) fn parse_object_record_value(
    expected_object_hash: ObjectHash,
    value: &Value,
) -> Result<ObjectRecord, StoreError> {
    let record: ObjectRecord = serde_json::from_value(value.clone())
        .map_err(|error| StoreError::InvalidData(error.to_string()))?;
    if record.object_hash != expected_object_hash {
        return Err(StoreError::InvalidData(format!(
            "object record key mismatch: path key '{}' does not match record object hash '{}'",
            expected_object_hash, record.object_hash
        )));
    }
    Ok(record)
}
