use crate::identity::{BuildKey, ResultId, ReuseKey};
use crate::{PublishedBuild, ResultRecord, ReuseInputIdentity, Store, StoreError};
use fsobj_hash::ObjectHash;
use std::path::{Path, PathBuf};

/// Request to publish a build under a publication name.
///
/// The staged path is a completed output object outside the permanent object
/// store. Publishing either reuses an existing build/result, or imports this
/// staged object, records the new result, and updates the publication refs.
#[derive(Debug)]
pub struct PublishRequest {
    /// Publication name to update under `object-refs` and `result-refs`.
    pub publication_name: String,
    /// Build key for the invocation being published.
    pub build_key: BuildKey,
    /// Reuse key for the invocation being published.
    pub reuse_key: ReuseKey,
    /// RFC 3339 timestamp recorded for a newly materialized result.
    pub created_at: String,
    /// Staged output object to import if reuse does not satisfy the request.
    pub staged_path: PathBuf,
    /// Realized input object identities to store with a newly materialized result.
    pub inputs: Vec<ReuseInputIdentity>,
}

/// Summary returned after a publication update completes.
///
/// The values identify the object and result now associated with the requested
/// build publication.
#[derive(Debug, Clone, Copy)]
pub struct Publication {
    /// Hash of the published object.
    pub object_hash: ObjectHash,
    /// Build key associated with the publication.
    pub build_key: BuildKey,
    /// Result id associated with the publication.
    pub result_id: ResultId,
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
/// [`PublishRequest::publication_name`] are updated to the selected result.
/// If the staged object is not needed because reuse succeeded, it is removed.
pub fn publish_build(store: &Store, request: PublishRequest) -> Result<Publication, StoreError> {
    if let Some(published) = crate::refs::load_build_handle(store, request.build_key)? {
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::publish_result(
            store,
            &request.publication_name,
            published.result.result_id(),
        )?;
        return Ok(Publication {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
            result_id: published.result.result_id(),
        });
    }

    if let Some(published) =
        crate::refs::resolve_reuse_for_build(store, request.build_key, request.reuse_key)?
    {
        let result_id = published.result.result_id();
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::publish_result(store, &request.publication_name, result_id)?;
        return Ok(Publication {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
            result_id,
        });
    }

    let published = materialize_build(
        store,
        request.build_key,
        request.reuse_key,
        &request.created_at,
        request.inputs,
        &request.staged_path,
    )?;
    crate::refs::publish_result(
        store,
        &request.publication_name,
        published.result.result_id(),
    )?;

    Ok(Publication {
        object_hash: published.build.object_hash,
        build_key: published.build.build_key,
        result_id: published.result.result_id(),
    })
}

/// Imports a staged object and records a newly materialized build result.
///
/// This lower-level operation does not update publication refs. It imports
/// `staged_path`, stores the result record, writes the reuse ref, and writes the
/// build handle ref. Call [`publish_build`] when the caller also needs to
/// update a publication name under `object-refs` and `result-refs`.
pub fn materialize_build(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    created_at: &str,
    inputs: Vec<ReuseInputIdentity>,
    staged_path: &Path,
) -> Result<PublishedBuild, StoreError> {
    materialize_build_impl(
        store,
        build_key,
        reuse_key,
        created_at,
        inputs,
        staged_path,
        None,
    )
}

/// Imports a staged object under a trusted precomputed hash.
///
/// This is a workspace-internal fast path for builders that have already
/// validated the staged output against a canonical model. Normal callers should
/// use [`materialize_build`], which hashes `staged_path` inside the store API.
#[doc(hidden)]
pub fn materialize_build_with_trusted_hash(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    created_at: &str,
    inputs: Vec<ReuseInputIdentity>,
    staged_path: &Path,
    trusted_object_hash: ObjectHash,
) -> Result<PublishedBuild, StoreError> {
    materialize_build_impl(
        store,
        build_key,
        reuse_key,
        created_at,
        inputs,
        staged_path,
        Some(trusted_object_hash),
    )
}

fn materialize_build_impl(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    created_at: &str,
    inputs: Vec<ReuseInputIdentity>,
    staged_path: &Path,
    object_hash: Option<ObjectHash>,
) -> Result<PublishedBuild, StoreError> {
    let object_hash = crate::object::import_object_with_hash(store, staged_path, object_hash)?;
    let result_id = crate::identity::compute_result_id(object_hash);
    let result = ResultRecord {
        object_hash,
        created_at: Some(created_at.to_string()),
        inputs,
    };
    crate::record::store_result_record(store, &result)?;
    crate::refs::store_reuse_ref(store, reuse_key, result_id)?;
    crate::refs::store_build_handle_ref(store, build_key, result_id)?;

    Ok(PublishedBuild {
        object_path: store.object_path(object_hash),
        build: crate::record::build_from_result(build_key, &result),
        result,
    })
}
