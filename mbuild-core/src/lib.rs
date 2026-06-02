use std::fmt;

pub mod builder;
pub mod cancellation;
pub mod fs_tree_compose;
pub mod fs_tree_manifest;
pub mod fs_tree_object;
pub mod initramfs;
pub mod logging;
pub mod origin;
#[doc(hidden)]
pub mod runtime_helper_protocol;

pub use builder::*;
pub use cancellation::*;
pub use cas::*;
pub use fs_tree_compose::*;
pub use fs_tree_manifest::*;
pub use fs_tree_object::*;
pub use fsobj_hash::ObjectHash;
pub use initramfs::*;
pub use logging::*;
pub use origin::*;
pub use runtime_helper_protocol::FsTreeArchiveEntrySource;

pub mod cas {
    pub use mbuild_store::{
        Build, BuildKey, CasError, ParseBuildKeyError, PublishOutputRequest, PublishedBuild,
        PublishedOutput, RealizedResult, ResultId, ResultRecord, ReuseInputIdentity, ReuseKey,
        StoreLayout, build_ref_path, compute_build_key, compute_result_id, compute_reuse_key,
        import_object, load_build_handle, load_public_build, load_result_record, load_reuse_record,
        materialize_build, object_path, publish_output, publish_refs, publish_result_refs,
        recreate_store_temp_dir_force, remove_store_temp_dir_force, result_path, reuse_ref_path,
        store_build_handle_ref, store_result_record, store_reuse_ref,
    };
}

#[derive(Debug)]
pub enum BuilderError {
    InvalidRecipe(String),
    Cancelled(String),
    ExecutionFailed(String),
    NotImplemented(String),
}

impl fmt::Display for BuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipe(message)
            | Self::Cancelled(message)
            | Self::ExecutionFailed(message)
            | Self::NotImplemented(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BuilderError {}
