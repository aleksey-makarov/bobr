mod error;
mod id;
mod json;
mod key;
mod layout;
mod object;
mod publish;
mod record;
mod refs;

pub use error::CasError;
pub use id::{BuildKey, ParseBuildKeyError, ResultId, ReuseKey};
pub use key::{compute_build_key, compute_result_id, compute_reuse_key};
pub use layout::{StoreLayout, recreate_store_temp_dir_force, remove_store_temp_dir_force};
pub use object::{import_object, object_path};
pub use publish::{PublishOutputRequest, PublishedOutput, materialize_build, publish_output};
pub use record::{
    Build, PublishedBuild, RealizedResult, ResultRecord, ReuseInputIdentity, load_result_record,
    store_result_record,
};
pub use refs::{
    build_ref_path, load_build_handle, load_public_build, load_reuse_record, publish_refs,
    publish_result_refs, result_path, reuse_ref_path, store_build_handle_ref, store_reuse_ref,
};

#[cfg(test)]
pub(crate) use json::canonical_json_bytes;
#[cfg(test)]
pub(crate) use layout::{OBJECTS_DIR, RESULTS_DIR};
#[cfg(test)]
pub(crate) use record::{BUILD_SCHEMA, RESULT_SCHEMA, build_json_value, parse_result_record_value};
#[cfg(test)]
pub(crate) use refs::{human_timestamp_from_rfc3339, load_current_publication, replace_symlink};

#[cfg(test)]
mod tests;
