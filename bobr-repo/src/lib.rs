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
mod encoding;
mod error;
mod format;
mod index;
mod master;
mod publisher;
mod reader;
mod s3;
mod tar_profile;
mod tls_config;
mod transport;

pub use cache::*;
pub(crate) use cbor::{Decoder, Encoder};
pub use content::*;
pub use encoding::{encode_preferred_fs_files, encode_preferred_object};
pub use error::RepositoryError;
pub use format::*;
pub use index::*;
pub use master::*;
pub use publisher::*;
pub use reader::*;
pub use s3::*;
pub use tar_profile::*;
pub use tls_config::*;
pub use transport::*;

/// Returns namespace runtime functions required by the repository publisher.
pub fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    vec![bobr_runtime::runtime_ns::NsFunction::new(
        encoding::EncodeFsFilesFunction,
    )]
}
