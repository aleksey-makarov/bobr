//! Repository format constants and typed metadata digests.

use crate::RepositoryError;
use sha2::{Digest, Sha256};
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

/// Repository wire-format version implemented by this crate.
pub const REPOSITORY_FORMAT: u64 = 1;

/// Maximum accepted encoded master size.
pub const MAX_MASTER_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum accepted immutable index or content-list size.
pub const MAX_METADATA_BYTES: u64 = 1024 * 1024 * 1024;
/// Maximum accepted encoded repository object or fs-file size.
pub const MAX_ENCODED_CONTENT_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
/// Maximum accepted decoded repository object or fs-file size.
pub const MAX_DECODED_CONTENT_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
/// Maximum number of logical entries in one directory object.
pub const MAX_TAR_ENTRIES: u64 = 4 * 1024 * 1024;
/// Maximum normalized tar path length in bytes.
pub const MAX_TAR_PATH_BYTES: usize = 64 * 1024;
/// Maximum individual tar path component length in bytes.
pub const MAX_TAR_COMPONENT_BYTES: usize = 255;
/// Maximum symbolic-link target length in bytes.
pub const MAX_TAR_SYMLINK_TARGET_BYTES: usize = 4095;
/// Maximum normalized directory-tree depth.
pub const MAX_TAR_DEPTH: usize = 1024;
/// Maximum sum of normalized path and symlink-target bytes in one tar.
pub const MAX_TAR_STRUCTURAL_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum Zstandard decoder window accepted by repository format version 1.
pub const MAX_ZSTD_WINDOW_BYTES: u64 = 128 * 1024 * 1024;

/// SHA-256 digest of one typed immutable repository metadata object.
pub struct MetadataHash<T> {
    bytes: [u8; 32],
    marker: PhantomData<fn() -> T>,
}

impl<T> MetadataHash<T> {
    /// Computes the typed metadata digest of `bytes`.
    pub fn digest(bytes: &[u8]) -> Self {
        Self::from_bytes(Sha256::digest(bytes).into())
    }

    /// Constructs a typed digest from raw SHA-256 bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            bytes,
            marker: PhantomData,
        }
    }

    /// Returns the raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Returns the lowercase hexadecimal representation used in URLs.
    pub fn to_hex(self) -> String {
        hex_encode(&self.bytes)
    }
}

impl<T> Clone for MetadataHash<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for MetadataHash<T> {}

impl<T> PartialEq for MetadataHash<T> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl<T> Eq for MetadataHash<T> {}

impl<T> std::hash::Hash for MetadataHash<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}

impl<T> PartialOrd for MetadataHash<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for MetadataHash<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bytes.cmp(&other.bytes)
    }
}

impl<T> fmt::Debug for MetadataHash<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("MetadataHash")
            .field(&hex_encode(&self.bytes))
            .finish()
    }
}

impl<T> fmt::Display for MetadataHash<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex_encode(&self.bytes))
    }
}

impl<T> FromStr for MetadataHash<T> {
    type Err = RepositoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_bytes(hex_decode(value)?))
    }
}

pub(crate) fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(value: &str) -> Result<[u8; 32], RepositoryError> {
    if value.len() != 64 {
        return Err(RepositoryError::new(
            "metadata hash must contain 64 lowercase hexadecimal digits",
        ));
    }
    let mut bytes = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(bytes)
}

fn hex_nibble(byte: u8) -> Result<u8, RepositoryError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(RepositoryError::new(
            "metadata hash must contain only lowercase hexadecimal digits",
        )),
    }
}
