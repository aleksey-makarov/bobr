//! Store ownership, object publication, and store metadata for `bobr`.
//!
//! This crate is the public boundary for operations that create, inspect, or
//! mutate a `bobr` store. It owns store initialization, object import,
//! build/reuse identifiers, object records, publication references, and the
//! future manifest-addressed `fs-tree` storage API.
//!
//! The crate intentionally does not provide general-purpose filesystem
//! utilities. Public functions are expressed in store terms: importing an
//! object, computing a store key, publishing checked objects, resolving reuse,
//! or scanning/materializing an `fs-tree`.
//!
//! Most fallible store operations return [`StoreError`]. Pure string parsing
//! for value types keeps narrow parse errors such as
//! [`mbuild_core::ParseHexHashError`] and [`fs_tree::ParseFsFileHashError`].

#![deny(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod error;
pub mod fs_tree;
mod fsutil;
mod json;
mod object;
mod publish;
mod record;
mod refs;
mod source;
mod store;

pub use error::StoreError;
pub use object::import_object;
#[doc(hidden)]
pub use publish::materialize_build_with_trusted_hash;
pub use publish::{Publication, PublishRequest, materialize_build, publish_build};
pub use record::{
    Build, ObjectRecord, PublishedBuild, RealizedObject, StoredObjectRecord, load_object_record,
    load_stored_object_record,
};
pub use refs::{
    load_build_handle, load_public_build, load_publication, publish_stored_object,
    resolve_reuse_for_build,
};
pub use source::{SourceImportOutcome, import_source_object, record_existing_source_object};
pub use store::{
    QuarantinedStoreTemp, Store, StoreRunLogLocations, StoreTempQuarantineRequest, StoreWorkspace,
    create_workspace, quarantine_store_temp, recreate_store_temp_dir_force,
    remove_store_temp_dir_force,
};

#[cfg(test)]
pub(crate) use json::canonical_json_bytes;
#[cfg(test)]
pub(crate) use record::{
    OBJECT_RECORD_SCHEMA, OBJECT_RECORD_SCHEMA_FOR_TEST, build_json_value,
    parse_object_record_value,
};
#[cfg(test)]
pub(crate) use refs::{load_current_publication, load_reuse_object_record, replace_symlink};
#[cfg(test)]
pub(crate) use store::{LOGS_DIR, OBJECT_RECORDS_DIR, OBJECTS_DIR, TMP_DIR};

#[cfg(test)]
mod tests;
