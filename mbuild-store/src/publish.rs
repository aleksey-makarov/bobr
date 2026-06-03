use crate::{
    BuildKey, PublishedBuild, ResultId, ResultRecord, ReuseInputIdentity, ReuseKey, Store,
    StoreError,
};
use fsobj_hash::ObjectHash;
use std::path::{Path, PathBuf};

/// Request to publish a staged build output.
///
/// The staged path is a completed output object outside the permanent object
/// store. Publishing either reuses an existing build/result, or imports this
/// staged object and records the new result.
#[derive(Debug)]
pub struct PublishOutputRequest {
    /// Public output name to update under `object-refs` and `result-refs`.
    pub output_name: String,
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

/// Summary returned after an output publication completes.
///
/// The values identify the object and result now associated with the requested
/// build/output publication.
#[derive(Debug, Clone, Copy)]
pub struct PublishedOutput {
    /// Hash of the published output object.
    pub object_hash: ObjectHash,
    /// Build key associated with the publication.
    pub build_key: BuildKey,
    /// Result id associated with the publication.
    pub result_id: ResultId,
}

/// Publishes a staged output or reuses an existing result.
///
/// The operation checks, in order:
///
/// 1. An existing build handle for `request.build_key`.
/// 2. An existing reuse record for `request.reuse_key`.
/// 3. The staged object supplied in `request.staged_path`.
///
/// In all successful cases the public output refs named by
/// [`PublishOutputRequest::output_name`] are updated to the selected result.
/// If the staged object is not needed because reuse succeeded, it is removed.
pub fn publish_output(
    store: &Store,
    request: PublishOutputRequest,
) -> Result<PublishedOutput, StoreError> {
    if let Some(published) = crate::refs::load_build_handle(store, request.build_key)? {
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::publish_result_refs(store, &request.output_name, &published.result)?;
        return Ok(PublishedOutput {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
            result_id: published.result.result_id(),
        });
    }

    if let Some(result) = crate::refs::load_reuse_record(store, request.reuse_key)? {
        let result_id = result.result_id();
        let object_path = store.object_path(result.object_hash);
        if !object_path.exists() {
            return Err(StoreError::Io(format!(
                "result '{}' points to missing object '{}'",
                result_id,
                object_path.display()
            )));
        }
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::store_build_handle_ref(store, request.build_key, result_id)?;
        let published = PublishedBuild {
            build: crate::record::build_from_result(request.build_key, &result),
            result,
            object_path,
        };
        crate::refs::publish_result_refs(store, &request.output_name, &published.result)?;
        return Ok(PublishedOutput {
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
        None,
    )?;
    crate::refs::publish_result_refs(store, &request.output_name, &published.result)?;

    Ok(PublishedOutput {
        object_hash: published.build.object_hash,
        build_key: published.build.build_key,
        result_id: published.result.result_id(),
    })
}

/// Imports a staged object and records a newly materialized build result.
///
/// This lower-level operation does not update public output refs. It imports
/// `staged_path`, stores the result record, writes the reuse ref, and writes the
/// build handle ref. Call [`publish_output`] when the caller also needs to
/// update an output name under `object-refs` and `result-refs`.
///
/// When `precomputed_object_hash` is supplied, the staged object is imported
/// under that hash without hashing it again.
pub fn materialize_build(
    store: &Store,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    created_at: &str,
    inputs: Vec<ReuseInputIdentity>,
    staged_path: &Path,
    precomputed_object_hash: Option<ObjectHash>,
) -> Result<PublishedBuild, StoreError> {
    let object_hash =
        crate::object::import_object_with_hash(store, staged_path, precomputed_object_hash)?;
    let result_id = crate::key::compute_result_id(object_hash)?;
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
