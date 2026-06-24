use crate::fsutil as private_fs;
use crate::{Store, StoreError};
use bobr_core::{BuildKey, ObjectHash};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

pub(crate) const OBJECT_RECORD_SCHEMA: &str = "bobr-object-record-v3";

/// Schema marker for the object record format. It (de)serializes only as the
/// current schema string, so the version is enforced declaratively and never
/// needs to live as data on `ObjectRecord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectRecordSchemaV3;

impl Serialize for ObjectRecordSchemaV3 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(OBJECT_RECORD_SCHEMA)
    }
}

impl<'de> Deserialize<'de> for ObjectRecordSchemaV3 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            OBJECT_RECORD_SCHEMA => Ok(ObjectRecordSchemaV3),
            other => Err(D::Error::custom(format!(
                "unsupported object record schema '{other}'"
            ))),
        }
    }
}

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
/// Object records are stored as JSON under the store's object record directory
/// and are keyed by [`ObjectRecord::object_hash`], not by the build key that
/// first produced them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRecord {
    /// Schema marker; enforces the record format version at (de)serialization.
    pub(crate) schema: ObjectRecordSchemaV3,
    /// Hash of the output object this record describes.
    pub object_hash: ObjectHash,
    /// Optional store run id that recorded this object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// Loads an object record and verifies that its object exists in the store.
///
/// Returns `Ok(None)` when the record file does not exist. Existing records are
/// parsed as object record JSON and must point to an existing object.
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
        schema: ObjectRecordSchemaV3,
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
    let json = serde_json::to_string(record).map_err(|error| {
        StoreError::InvalidData(format!("failed to encode object record JSON: {error}"))
    })?;
    private_fs::write_atomic(&object_record_path, &json).map_err(crate::error::map_fsutil_error)
}

pub(crate) fn build_from_object_record(build_key: BuildKey, object_record: &ObjectRecord) -> Build {
    Build {
        build_key,
        object_hash: object_record.object_hash,
        run_id: object_record.run_id.clone(),
    }
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
