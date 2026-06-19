use crate::{ObjectRecord, PublishedBuild, Store, StoreError};
use mbuild_core::{BuildKey, ObjectHash, ReuseKey};
use std::path::{Path, PathBuf};

/// Request to publish a build under a publication name.
///
/// The staged path is a completed output object outside the permanent object
/// store. Publishing either reuses an existing build/object-record pair, or
/// imports this staged object, records the new object record, and updates the
/// publication refs.
#[derive(Debug)]
pub struct PublishRequest {
    /// Publication name to update under `object-refs` and `object-record-refs`.
    pub publication_name: String,
    /// Build key for the invocation being published.
    pub build_key: BuildKey,
    /// Reuse key for the invocation being published.
    pub reuse_key: ReuseKey,
    /// Staged output object to import if reuse does not satisfy the request.
    pub staged_path: PathBuf,
    /// Realized input object hashes to store with a newly materialized object.
    pub inputs: Vec<ObjectHash>,
}

/// Summary returned after a publication update completes.
///
/// The values identify the object now associated with the requested build
/// publication.
#[derive(Debug, Clone, Copy)]
pub struct Publication {
    /// Hash of the published object.
    pub object_hash: ObjectHash,
    /// Build key associated with the publication.
    pub build_key: BuildKey,
}

/// Publishes a build under a publication name.
///
/// The operation checks, in order:
///
/// 1. An existing build handle for `request.build_key`.
/// 2. An existing reuse record for `request.reuse_key`.
/// 3. The staged object supplied in `request.staged_path`.
///
/// In all successful cases the publication refs named by
/// [`PublishRequest::publication_name`] are updated to the selected object.
/// If the staged object is not needed because reuse succeeded, it is removed.
pub fn publish_build(store: &Store, request: PublishRequest) -> Result<Publication, StoreError> {
    if let Some(published) = crate::refs::load_build_handle(store, request.build_key)? {
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::publish_stored_object(
            store,
            &request.publication_name,
            published.object_record.object_hash,
        )?;
        return Ok(Publication {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
        });
    }

    if let Some(published) =
        crate::refs::resolve_reuse_for_build(store, request.build_key, request.reuse_key)?
    {
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::publish_stored_object(
            store,
            &request.publication_name,
            published.object_record.object_hash,
        )?;
        return Ok(Publication {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
        });
    }

    let published = materialize_build(
        store,
        request.build_key,
        request.reuse_key,
        request.inputs,
        &request.staged_path,
    )?;
    crate::refs::publish_stored_object(
        store,
        &request.publication_name,
        published.object_record.object_hash,
    )?;

    Ok(Publication {
        object_hash: published.build.object_hash,
        build_key: published.build.build_key,
    })
}

/// Imports a staged object and records a newly materialized build object.
///
/// This lower-level operation does not update publication refs. It imports
/// `staged_path`, stores the object record, writes the reuse ref, and writes
/// the build handle ref. Call [`publish_build`] when the caller also needs to
/// update a publication name under `object-refs` and `object-record-refs`.
pub fn materialize_build(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    inputs: Vec<ObjectHash>,
    staged_path: &Path,
) -> Result<PublishedBuild, StoreError> {
    let object_hash = crate::object::import_object(store, staged_path)?;
    let object_record = ObjectRecord {
        object_hash,
        run_id: Some(store.run_id().to_string()),
        inputs,
    };
    crate::record::store_object_record(store, &object_record)?;
    crate::refs::store_reuse_ref(store, reuse_key, object_hash)?;
    crate::refs::store_build_handle_ref(store, build_key, object_hash)?;

    Ok(PublishedBuild {
        object_path: store.object_path(object_hash),
        build: crate::record::build_from_object_record(build_key, &object_record),
        object_record,
    })
}
