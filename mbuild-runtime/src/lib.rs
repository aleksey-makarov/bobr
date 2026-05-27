//! Runtime support for idmap-backed builder operations.
//!
//! `mbuild-runtime` exposes builder-facing runtime capabilities. Public
//! callers should use the facade functions exported from this crate root.
//! Helper launch, sandbox bootstrap, and child-side error reporting are
//! internal details.
//!
//! The current public capabilities are fs-tree ownership materialization
//! through [`apply_ownership_batch`], deterministic fs-tree tar and initramfs
//! generation, and `Sandbox` execution through [`run_sandbox_build`]. These
//! helpers operate in a user namespace described by [`MbuildIdmap`].
//!
//! The public `Sandbox` runtime capability accepts a prepared root filesystem
//! directory through [`SandboxBuildConfig::root_dir`], mounts extra
//! [`SandboxInput`] values under `/__mbuild/inputs/<name>`, runs ordered
//! [`SandboxStep`] commands, and returns the scanned fs-tree output manifest.
//!
//! Runtime ownership materialization currently targets Linux hosts with
//! configured `/etc/subuid` and `/etc/subgid` ranges, unprivileged user
//! namespace support, and executable `newuidmap`/`newgidmap` helpers.

#![deny(missing_docs)]

mod archive_writer;
mod error;
mod idmap;
mod initramfs_writer;
mod local_helper;
mod local_ownership;
mod ownership;
mod preflight;
mod sandbox;
mod tar_writer;

mod executor;

pub use archive_writer::FsTreeArchiveInput;
pub use error::{IdmapError, RuntimeError};
pub use idmap::{MbuildIdmap, cached_host_idmap};
pub use initramfs_writer::write_fs_tree_initramfs_in_ownership_namespace;
pub use mbuild_core::FsTreeArchiveEntrySource;
pub use ownership::apply_ownership_batch;
pub use sandbox::{
    SandboxBuildConfig, SandboxBuildOutcome, SandboxInput, SandboxRunAs, SandboxStep,
    SandboxStepReport, run_sandbox_build,
};
pub use tar_writer::write_fs_tree_tar_in_ownership_namespace;
