use std::fmt;

pub mod builder;
pub mod cancellation;
pub mod fs_tree_compose;
pub mod fs_tree_manifest;
pub mod fs_tree_object;
pub mod initramfs;
pub mod logging;
pub mod oci;
pub mod origin;
#[doc(hidden)]
pub mod runtime_helper_protocol;

pub use builder::*;
pub use cancellation::*;
pub use fs_tree_compose::*;
pub use fs_tree_manifest::*;
pub use fs_tree_object::*;
pub use fsobj_hash::ObjectHash;
pub use initramfs::*;
pub use logging::*;
pub use origin::*;
pub use runtime_helper_protocol::FsTreeArchiveEntrySource;

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
