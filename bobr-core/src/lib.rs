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

/// The date bobr pretends it is, as seconds since the epoch.
///
/// One answer to one question, used wherever a timestamp would otherwise record
/// when a build happened to run: the store stamps everything it owns with it, so
/// that two stores which admitted the same content at different moments hand a
/// builder the same tree; and the sandbox exports it as `SOURCE_DATE_EPOCH`, so
/// that builders which stamp their own output agree with it.
///
/// 1980-01-01 UTC, not 0: tools like groff's mdate.pl treat `SOURCE_DATE_EPOCH=0`
/// as unset (`$ENV{...} || mtime`; "0" is falsy in Perl) and fall back to the
/// build-time file mtime; pre-1980 dates also break DOS-derived tools (zip).
/// Matches nixpkgs.
pub const CANONICAL_TIMESTAMP: i64 = 315_532_800;

// These modules organize the code; their public items are re-exported at the
// crate root below, which is the single supported path.
mod cancellation;
pub mod fsutil;
mod identity;
mod logging;
mod run;
mod subject_run_context;
mod workspace;

/// OCI image-layout types and helpers (used as `bobr_core::oci::…`).
pub mod oci;

pub use cancellation::*;
pub use identity::*;
pub use logging::*;
pub use run::*;
pub use subject_run_context::*;
pub use workspace::*;
