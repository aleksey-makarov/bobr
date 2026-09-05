//! Bobr remote repository format, reader, and publisher.
//!
//! The crate owns the authenticated repository wire format and the shared
//! mechanisms used by both the asynchronous Bobr client and the repository
//! publication utility. Filesystem storage remains owned by `bobr-store`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod cache;
mod cbor;
mod content;
mod error;
mod format;
mod index;
mod master;
mod publisher;
mod reader;
mod tar_profile;
mod transport;

pub use cache::*;
pub(crate) use cbor::{Decoder, Encoder};
pub use content::*;
pub use error::RepositoryError;
pub use format::*;
pub use index::*;
pub use master::*;
pub use publisher::*;
pub use reader::*;
pub use tar_profile::*;
pub use transport::*;
