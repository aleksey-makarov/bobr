//! Runtime support for idmap-backed builder operations.
//!
//! `mbuild-runtime` exposes builder-facing runtime capabilities rather than
//! raw `libcontainer` primitives. Public callers should use the facade
//! functions exported from this crate root. OCI bundle construction, executor
//! lifecycle handling, and child-side error reporting are internal details.
//!
//! The current public capabilities are fs-tree ownership materialization through
//! [`apply_ownership_batch`], [`apply_ownership_batch_and_hash`], and
//! [`apply_selected_ownership_batch_and_hash_fs_tree_object`], plus deterministic
//! fs-tree tar generation through [`write_fs_tree_tar_in_ownership_namespace`].
//! These helpers operate in a user namespace described by [`MbuildIdmap`].
//!
//! Runtime ownership materialization currently targets Linux hosts with
//! configured `/etc/subuid` and `/etc/subgid` ranges, unprivileged user
//! namespace support, and a working `libcontainer` setup.

#![deny(missing_docs)]

mod bundle;
mod error;
mod idmap;
mod ownership;
mod preflight;
mod run;
mod sandbox;
mod spec;
mod tar_writer;

mod executor;

pub use error::{IdmapError, RuntimeError};
pub use idmap::{MbuildIdmap, cached_host_idmap};
pub use ownership::{
    apply_ownership_batch, apply_ownership_batch_and_hash,
    apply_ownership_batch_and_hash_fs_tree_object,
    apply_selected_ownership_batch_and_hash_fs_tree_object,
};
pub use sandbox::{
    SandboxBuildConfig, SandboxBuildOutcome, SandboxInput, SandboxRunAs, SandboxStep,
    SandboxStepReport, run_sandbox_build,
};
pub use tar_writer::{
    FsTreeTarEntrySource, FsTreeTarInput, write_fs_tree_tar_in_ownership_namespace,
};
