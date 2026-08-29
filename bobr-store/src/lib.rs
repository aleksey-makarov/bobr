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
//! [`bobr_core::ParseHexHashError`] and [`fs_tree::ParseFsFileHashError`].

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod copy_content;
mod error;
pub mod fs_tree;
mod local_content;
mod object;
mod record;
mod ref_name;
mod refs;
mod secondary;
mod secondary_resolver;
mod source;
mod store;

pub use copy_content::LocalCopyContentSource;
pub use error::StoreError;
pub use object::import_build;
pub use ref_name::validate_ref_name;
pub use refs::{load_build_object_hash, load_reuse_object_hash, publish_existing_build};
pub use secondary::{
    ContentImportOutcome, ContentSource, ContentTransferMode, LocalHardlinkContentSource,
    LocalRepository, LocalTrustedKeyIndex, TrustedKeyIndex, TrustedResolution,
};
pub use secondary_resolver::{
    ContentTransferEvent, ContentTransferReport, KnownObjectResolution, MappingCandidates,
    NamedContentSource, NamedTrustedKeyIndex, ResolvedSecondaryContent, ReuseQuery,
    SecondaryResolution, SecondaryResolver, TrustedAnswer,
};
pub use source::{SourceImportOutcome, import_source_object, record_existing_source_object};
pub use store::{ReadOnlyStore, Store};

/// Returns namespace runtime functions used by secondary-store imports.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![
        bobr_runtime::runtime_ns::NsFunction::new(secondary::HardlinkFsFilesFunction),
        bobr_runtime::runtime_ns::NsFunction::new(copy_content::CopyFsFilesFunction),
    ]
}

#[cfg(test)]
pub(crate) use object::import_object;
#[cfg(test)]
pub(crate) use refs::replace_symlink;
#[cfg(test)]
pub(crate) use store::OBJECTS_DIR;

#[cfg(test)]
mod tests;
