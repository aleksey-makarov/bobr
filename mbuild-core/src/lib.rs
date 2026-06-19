use std::fmt;

pub use bobr_runtime::runtime_provider::{RuntimeBackend, RuntimeProvider};

pub mod cancellation;
pub mod fs_tree_compose;
pub mod fs_tree_manifest;
pub mod fs_tree_object;
pub mod identity;
pub mod initramfs;
pub mod logging;
pub mod oci;
pub mod origin;
pub mod publication;
pub mod subject_run_context;
pub mod workspace;

pub use cancellation::*;
pub use fs_tree_compose::*;
pub use fs_tree_manifest::*;
pub use fs_tree_object::*;
pub use identity::*;
pub use initramfs::*;
pub use logging::*;
pub use origin::*;
pub use publication::*;
pub use subject_run_context::*;
pub use workspace::*;

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
