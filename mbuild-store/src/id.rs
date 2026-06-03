use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Stable key for a normalized build invocation.
///
/// A build key is the SHA-256 digest produced by [`crate::compute_build_key`].
/// It identifies the builder tag, normalized payload, and input build keys,
/// independent of whether the corresponding result has already been published.
///
/// The textual representation is exactly 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuildKey([u8; 32]);

impl BuildKey {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Formats the key as 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        hex_encode(self.0)
    }
}

/// Stable identifier for a realized result object.
///
/// Result ids are derived from the result object's [`fsobj_hash::ObjectHash`]
/// by [`crate::compute_result_id`]. A result id names the JSON result record
/// stored under the store's `results` directory.
///
/// The textual representation is exactly 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResultId([u8; 32]);

impl ResultId {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Formats the id as 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        hex_encode(self.0)
    }
}

/// Stable key used to reuse an existing result across equivalent inputs.
///
/// A reuse key is produced by [`crate::compute_reuse_key`] from the builder tag,
/// normalized payload, and input object identities. It maps to a [`ResultId`]
/// through the store's `reuses` references.
///
/// The textual representation is exactly 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReuseKey([u8; 32]);

impl ReuseKey {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Formats the key as 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        hex_encode(self.0)
    }
}

impl fmt::Display for BuildKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(self.0, f)
    }
}

impl fmt::Debug for BuildKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BuildKey").field(&self.to_hex()).finish()
    }
}

impl Serialize for BuildKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for BuildKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ResultId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(self.0, f)
    }
}

impl fmt::Debug for ResultId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ResultId").field(&self.to_hex()).finish()
    }
}

impl Serialize for ResultId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ResultId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ReuseKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(self.0, f)
    }
}

impl fmt::Debug for ReuseKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ReuseKey").field(&self.to_hex()).finish()
    }
}

impl Serialize for ReuseKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ReuseKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Error returned when parsing a 32-byte store id from text.
///
/// This error is used by [`BuildKey`], [`ResultId`], and [`ReuseKey`]. The name
/// is historical: all three value types share the same 64-character lowercase
/// hex encoding and therefore the same parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseBuildKeyError {
    /// The input length is not exactly 64 bytes.
    InvalidLength,
    /// The input contains a byte outside `[0-9a-f]`.
    InvalidHex,
}

impl fmt::Display for ParseBuildKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

impl std::error::Error for ParseBuildKeyError {}

impl FromStr for BuildKey {
    type Err = ParseBuildKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_hex_32(s).map(Self)
    }
}

impl FromStr for ResultId {
    type Err = ParseBuildKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_hex_32(s).map(Self)
    }
}

impl FromStr for ReuseKey {
    type Err = ParseBuildKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_hex_32(s).map(Self)
    }
}

fn hex_encode(bytes: [u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn write_hex(bytes: [u8; 32], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

fn parse_hex_32(value: &str) -> Result<[u8; 32], ParseBuildKeyError> {
    if value.len() != 64 {
        return Err(ParseBuildKeyError::InvalidLength);
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ParseBuildKeyError::InvalidHex);
    }

    let mut bytes = [0u8; 32];
    for (idx, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let hi = decode_nibble(chunk[0]).ok_or(ParseBuildKeyError::InvalidHex)?;
        let lo = decode_nibble(chunk[1]).ok_or(ParseBuildKeyError::InvalidHex)?;
        bytes[idx] = (hi << 4) | lo;
    }
    Ok(bytes)
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
