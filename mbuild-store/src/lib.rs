//! Store ownership, object publication, and store metadata for `mbuild`.
//!
//! This crate is the public boundary for operations that create, inspect, or
//! mutate an `mbuild` store. It owns store initialization, object import,
//! build/result/reuse identifiers, result records, publication references, and
//! the future manifest-addressed `fs-tree` storage API.
//!
//! The crate intentionally does not provide general-purpose filesystem
//! utilities. Public functions are expressed in store terms: importing an
//! object, computing a store key, storing a result record, publishing refs, or
//! scanning/materializing an `fs-tree`.
//!
//! Most fallible store operations return [`StoreError`]. Pure string parsing
//! for value types keeps narrow parse errors such as [`ParseBuildKeyError`] and
//! [`fs_tree::ParseFsFileHashError`].

#![deny(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("mbuild requires Linux");

mod error;
pub mod fs_tree;
mod fsutil;
mod id;
mod json;
mod key;
mod object;
mod publish;
mod record;
mod refs;
mod store;

pub use error::StoreError;
pub use fsobj_hash::ObjectHash;
pub use id::{BuildKey, ParseBuildKeyError, ResultId, ReuseKey};
pub use key::{compute_build_key, compute_result_id, compute_reuse_key};
pub use object::import_object;
pub use publish::{PublishOutputRequest, PublishedOutput, materialize_build, publish_output};
pub use record::{
    Build, PublishedBuild, RealizedResult, ResultRecord, ReuseInputIdentity, load_result_record,
    store_result_record,
};
pub use refs::{
    load_build_handle, load_public_build, load_reuse_record, publish_refs, publish_result_refs,
    store_build_handle_ref, store_reuse_ref,
};
pub use store::{Store, recreate_store_temp_dir_force, remove_store_temp_dir_force};

#[cfg(test)]
pub(crate) use json::canonical_json_bytes;
#[cfg(test)]
pub(crate) use record::{BUILD_SCHEMA, RESULT_SCHEMA, build_json_value, parse_result_record_value};
#[cfg(test)]
pub(crate) use refs::{human_timestamp_from_rfc3339, load_current_publication, replace_symlink};
#[cfg(test)]
pub(crate) use store::{OBJECTS_DIR, RESULTS_DIR};

#[cfg(test)]
mod tests;
