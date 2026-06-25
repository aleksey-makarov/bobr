use crate::record::ObjectRecordSchemaV4;
use crate::{ObjectRecord, PublishedBuild, Store, StoreError};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use std::path::Path;

/// Imports a staged object and records a newly materialized build object.
///
/// The operation imports `staged_path`, stores the object record, writes the
/// reuse ref, writes the build handle ref, and optionally updates
/// `object-refs/<name>` for the successfully materialized object.
pub fn materialize_build(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    inputs: Vec<ObjectHash>,
    staged_path: &Path,
    object_ref_name: Option<&str>,
) -> Result<PublishedBuild, StoreError> {
    if let Some(name) = object_ref_name {
        crate::validate_ref_name(name)?;
    }
    let object_hash = crate::object::import_object(store, staged_path)?;
    let object_record = ObjectRecord {
        schema: ObjectRecordSchemaV4,
        build_key,
        object_hash,
        run_id: Some(store.run_id().to_string()),
        inputs,
    };
    crate::record::store_object_record(store, &object_record)?;
    crate::refs::store_reuse_ref(store, reuse_key, object_hash)?;
    crate::refs::store_build_handle_ref(store, build_key, object_hash)?;
    if let Some(name) = object_ref_name {
        crate::refs::update_object_ref(store, name, object_hash)?;
    }

    Ok(PublishedBuild {
        object_path: store.object_path(object_hash),
        build: crate::record::build_from_object_record(build_key, &object_record),
        object_record,
    })
}
