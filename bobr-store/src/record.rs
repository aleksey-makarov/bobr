use crate::{Store, StoreError};
use bobr_core::fsutil as private_fs;
use bobr_core::{BuildKey, ObjectHash};
use serde::{Serialize, Serializer};

pub(crate) const OBJECT_RECORD_SCHEMA: &str = "bobr-object-record-v4";

/// Schema marker serialized as the current write-only record format string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectRecordSchemaV4;

impl Serialize for ObjectRecordSchemaV4 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(OBJECT_RECORD_SCHEMA)
    }
}

/// Write-only store record for a realized object.
///
/// Object records are stored as JSON under the store's object record directory
/// and are keyed by their object hash. Build and reuse lookup deliberately do
/// not load this metadata; it remains a temporary write-only artifact until
/// its eventual store-layout redesign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ObjectRecord {
    /// Schema marker written into the record.
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

/// Records an object already present in the store with neutral local metadata.
///
/// The object for `object_hash` must exist; its object record is written
/// idempotently.
pub(crate) fn record_existing_object(
    store: &Store,
    object_hash: ObjectHash,
    run_id: &str,
) -> Result<(), StoreError> {
    if !store.object_is_complete(object_hash)? {
        return Err(StoreError::Io(format!(
            "object '{object_hash}' is incomplete in store"
        )));
    }

    let object_record = ObjectRecord {
        schema: ObjectRecordSchemaV4,
        build_key: BuildKey::from_object_hash(object_hash),
        object_hash,
        run_id: Some(run_id.to_string()),
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
