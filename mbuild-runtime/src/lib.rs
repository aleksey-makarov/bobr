//! Runtime support for idmap-backed builder operations.
//!
//! `mbuild-runtime` exposes builder-facing runtime capabilities. Public
//! callers should use the facade functions exported from this crate root.
//! Helper launch, sandbox bootstrap, and child-side error reporting are
//! internal details.
//!
//! The current public capabilities are fs-tree ownership materialization
//! through [`apply_ownership_batch`], deterministic fs-tree tar and initramfs
//! generation, and `Sandbox` execution through [`run_sandbox_build`].
//! Runtime operations resolve the cached host idmap internally.
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
mod materialize;
mod ownership;
mod preflight;
mod sandbox;
mod tar_writer;

mod executor;

pub use archive_writer::FsTreeArchiveInput;
pub use error::{IdmapError, RuntimeError};
pub use initramfs_writer::write_fs_tree_initramfs_in_ownership_namespace;
pub use materialize::materialize_fs_tree_from_sources_in_ownership_namespace;
pub use mbuild_core::FsTreeArchiveEntrySource;
pub use mbuild_core::runtime_helper_protocol::FsTreeMaterializeReport;
pub use ownership::apply_ownership_batch;
#[cfg(unix)]
pub use ownership::validate_fs_tree_file_attrs_in_ownership_namespace;
pub use sandbox::{
    DEFAULT_SANDBOX_UMASK, SandboxBuildConfig, SandboxBuildOutcome, SandboxInput, SandboxRunAs,
    SandboxStep, SandboxStepReport, run_sandbox_build,
};
pub use tar_writer::write_fs_tree_tar_in_ownership_namespace;
