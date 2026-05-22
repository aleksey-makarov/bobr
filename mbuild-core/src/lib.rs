use std::fmt;

pub mod builder;
pub mod cancellation;
pub mod cas;
pub mod fs_tree_compose;
pub mod fs_tree_manifest;
pub mod fs_tree_object;
pub mod fsutil;
pub mod initramfs;
pub mod origin;

pub use builder::*;
pub use cancellation::*;
pub use cas::*;
pub use fs_tree_compose::*;
pub use fs_tree_manifest::*;
pub use fs_tree_object::*;
pub use fsobj_hash::ObjectHash;
pub use initramfs::*;
pub use origin::*;

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
