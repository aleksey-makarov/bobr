//! Asynchronous acquisition of pinned Source content for the unified Realizer.

pub(crate) mod engine;
mod limits;
mod oci;

pub use limits::Limits;
pub(crate) use limits::ResolvedLimits;
