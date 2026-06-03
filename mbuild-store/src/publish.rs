use crate::{
    BuildKey, CasError, PublishedBuild, ResultId, ResultRecord, ReuseInputIdentity, ReuseKey,
    StoreLayout,
};
use fsobj_hash::ObjectHash;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct PublishOutputRequest {
    pub output_name: String,
    pub build_key: BuildKey,
    pub reuse_key: ReuseKey,
    pub created_at: String,
    pub staged_path: PathBuf,
    pub inputs: Vec<ReuseInputIdentity>,
}

#[derive(Debug, Clone, Copy)]
pub struct PublishedOutput {
    pub object_hash: ObjectHash,
    pub build_key: BuildKey,
    pub result_id: ResultId,
}

pub fn publish_output(
    layout: &StoreLayout,
    request: PublishOutputRequest,
) -> Result<PublishedOutput, CasError> {
    if let Some(published) = crate::refs::load_build_handle(layout, request.build_key)? {
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::publish_result_refs(layout, &request.output_name, &published.result)?;
        return Ok(PublishedOutput {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
            result_id: published.result.result_id(),
        });
    }

    if let Some(result) = crate::refs::load_reuse_record(layout, request.reuse_key)? {
        let result_id = result.result_id();
        let object_path = crate::object::object_path(layout, result.object_hash);
        if !object_path.exists() {
            return Err(CasError::Io(format!(
                "result '{}' points to missing object '{}'",
                result_id,
                object_path.display()
            )));
        }
        crate::object::remove_path_force(&request.staged_path)?;
        crate::refs::store_build_handle_ref(layout, request.build_key, result_id)?;
        let published = PublishedBuild {
            build: crate::record::build_from_result(request.build_key, &result),
            result,
            object_path,
        };
        crate::refs::publish_result_refs(layout, &request.output_name, &published.result)?;
        return Ok(PublishedOutput {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
            result_id,
        });
    }

    let published = materialize_build(
        layout,
        request.build_key,
        request.reuse_key,
        &request.created_at,
        request.inputs,
        &request.staged_path,
        None,
    )?;
    crate::refs::publish_result_refs(layout, &request.output_name, &published.result)?;

    Ok(PublishedOutput {
        object_hash: published.build.object_hash,
        build_key: published.build.build_key,
        result_id: published.result.result_id(),
    })
}

pub fn materialize_build(
    layout: &StoreLayout,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    created_at: &str,
    inputs: Vec<ReuseInputIdentity>,
    staged_path: &Path,
    precomputed_object_hash: Option<ObjectHash>,
) -> Result<PublishedBuild, CasError> {
    let object_hash =
        crate::object::import_object_with_hash(layout, staged_path, precomputed_object_hash)?;
    let result_id = crate::key::compute_result_id(object_hash)?;
    let result = ResultRecord {
        object_hash,
        created_at: Some(created_at.to_string()),
        inputs,
    };
    crate::record::store_result_record(layout, &result)?;
    crate::refs::store_reuse_ref(layout, reuse_key, result_id)?;
    crate::refs::store_build_handle_ref(layout, build_key, result_id)?;

    Ok(PublishedBuild {
        object_path: layout.objects.join(object_hash.to_hex()),
        build: crate::record::build_from_result(build_key, &result),
        result,
    })
}
