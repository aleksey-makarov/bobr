//! Runtime support for idmap-backed builder operations.
//!
//! `mbuild-runtime` exposes builder-facing runtime capabilities rather than
//! raw `libcontainer` primitives. Public callers should use the facade
//! functions exported from this crate root. OCI bundle construction, executor
//! lifecycle handling, and child-side error reporting are internal details.
//!
//! The current public capability is fs-tree ownership materialization through
//! [`apply_ownership_batch`]. It applies logical fs-tree owners and modes in a
//! user namespace described by [`MbuildIdmap`].
//!
//! Runtime ownership materialization currently targets Linux hosts with
//! configured `/etc/subuid` and `/etc/subgid` ranges, unprivileged user
//! namespace support, and a working `libcontainer` setup.

#![deny(missing_docs)]

mod bundle;
mod error;
mod idmap;
mod ownership;
mod run;
mod spec;

mod executor;

pub use error::{IdmapError, RuntimeError};
pub use idmap::{MbuildIdmap, cached_host_idmap};
pub use ownership::apply_ownership_batch;
