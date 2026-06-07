//! Store ownership, object publication, and store metadata for `bobr`.
//!
//! This crate is the public boundary for operations that create, inspect, or
//! mutate a `bobr` store. It owns store initialization, object import,
//! build/result/reuse identifiers, result records, publication references, and
//! the future manifest-addressed `fs-tree` storage API.
//!
//! The crate intentionally does not provide general-purpose filesystem
//! utilities. Public functions are expressed in store terms: importing an
//! object, computing a store key, publishing checked results, resolving reuse,
//! or scanning/materializing an `fs-tree`.
//!
//! Most fallible store operations return [`StoreError`]. Pure string parsing
//! for value types keeps narrow parse errors such as
//! [`identity::ParseIdentityError`] and [`fs_tree::ParseFsFileHashError`].

#![deny(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod error;
pub mod fs_tree;
mod fsutil;
pub mod identity;
mod json;
mod object;
mod publish;
mod record;
mod refs;
mod source;
mod store;

pub use error::StoreError;
pub use fsobj_hash::ObjectHash;
pub use object::import_object;
#[doc(hidden)]
pub use publish::materialize_build_with_trusted_hash;
pub use publish::{PublishOutputRequest, PublishedOutput, materialize_build, publish_output};
pub use record::{
    Build, PublishedBuild, RealizedResult, ResultRecord, ReuseInputIdentity, StoredResult,
    load_result_record, load_stored_result,
};
pub use refs::{
    load_build_handle, load_public_build, load_public_output, publish_result,
    resolve_reuse_for_build,
};
pub use source::{SourceImportOutcome, SourceLookup, import_source_result, lookup_source_result};
pub use store::{
    QuarantinedStoreTemp, Store, StoreRunLogLocations, StoreTempQuarantineRequest, StoreWorkspace,
    WorkspaceRequest, create_workspace, list_quarantined_temps, quarantine_store_temp,
    recreate_store_temp_dir_force, remove_store_temp_dir_force,
};

#[cfg(test)]
pub(crate) use json::canonical_json_bytes;
#[cfg(test)]
pub(crate) use record::{BUILD_SCHEMA, RESULT_SCHEMA, build_json_value, parse_result_record_value};
#[cfg(test)]
pub(crate) use refs::{
    human_timestamp_from_rfc3339, load_current_publication, load_reuse_record, replace_symlink,
};
#[cfg(test)]
pub(crate) use store::{LOGS_DIR, OBJECTS_DIR, RESULTS_DIR, TMP_DIR};

#[cfg(test)]
mod tests;
