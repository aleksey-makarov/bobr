//! Shared vocabulary for the bobr build system.
//!
//! Foundational types used across the builder, source, store, and execution
//! crates: subject identity, object hashes and build keys, the build event
//! logging subsystem, per-subject run context and workspace, the cancellation
//! token, and OCI image-layout helpers (the [`oci`] module). The runtime backend
//! selection ([`RuntimeProvider`]) is re-exported from `bobr-runtime`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

pub use bobr_runtime::runtime_provider::{RuntimeBackend, RuntimeProvider};

// These modules organize the code; their public items are re-exported at the
// crate root below, which is the single supported path.
mod cancellation;
mod identity;
mod logging;
mod subject_run_context;
mod workspace;

/// OCI image-layout types and helpers (used as `bobr_core::oci::…`).
pub mod oci;

pub use cancellation::*;
pub use identity::*;
pub use logging::*;
pub use subject_run_context::*;
pub use workspace::*;
