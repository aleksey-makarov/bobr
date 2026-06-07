//! Stable store identity computation.
//!
//! This module computes deterministic identifiers from normalized semantic
//! inputs: build keys for requested invocations, reuse keys for realized input
//! objects, and result ids for imported output objects.

use crate::{ReuseInputIdentity, StoreError};
use fsobj_hash::ObjectHash;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

const INVOCATION_SCHEMA: &str = "bobr-build-invocation-v1";
const RESULT_INVOCATION_SCHEMA: &str = "bobr-build-result-invocation-v3";

/// Error returned when parsing a 32-byte store id from text.
///
/// This error is used by [`BuildKey`], [`ResultId`], and [`ReuseKey`]. All
/// three value types share the same 64-character lowercase hex encoding and
/// therefore the same parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseIdentityError {
    /// The input length is not exactly 64 bytes.
    InvalidLength,
    /// The input contains a byte outside `[0-9a-f]`.
    InvalidHex,
}

impl fmt::Display for ParseIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

impl std::error::Error for ParseIdentityError {}

macro_rules! define_identity_type {
    (
        $(#[$meta:meta])*
        pub struct $name:ident;
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name([u8; 32]);

        impl $name {
            pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// Returns the raw 32-byte digest.
            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Formats the identity as 64 lowercase hexadecimal characters.
            pub fn to_hex(&self) -> String {
                hex_encode(self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name))
                    .field(&self.to_hex())
                    .finish()
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdentityError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                parse_hex_32(s).map(Self)
            }
        }
    };
}

define_identity_type! {
    /// Stable key for a normalized build invocation.
    ///
    /// A build key is the SHA-256 digest produced by [`compute_build_key`]. It
    /// identifies the builder tag, normalized payload, and input build keys,
    /// independent of whether the corresponding result has already been
    /// published.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct BuildKey;
}

define_identity_type! {
    /// Stable identifier for a realized result object.
    ///
    /// Result ids are derived from the result object's [`ObjectHash`] by
    /// [`compute_result_id`]. A result id names the JSON result record stored
    /// under the store's `results` directory.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct ResultId;
}

define_identity_type! {
    /// Stable key used to reuse an existing result across equivalent inputs.
    ///
    /// A reuse key is produced by [`compute_reuse_key`] from the builder tag,
    /// normalized payload, and input object identities. It maps to a
    /// [`ResultId`] through the store's `reuses` references.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct ReuseKey;
}

/// Computes the stable key for a normalized build invocation.
///
/// The key covers the builder tag, the normalized JSON payload, and the ordered
/// list of input build keys. The payload is serialized with the store's
/// canonical JSON encoder before hashing, so callers must pass the already
/// normalized semantic payload rather than arbitrary user input.
pub fn compute_build_key(
    builder_tag: &str,
    normalized_payload: &Value,
    input_build_keys: &[BuildKey],
) -> Result<BuildKey, StoreError> {
    let input_keys = input_build_keys
        .iter()
        .map(|key| Value::String(key.to_string()))
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(INVOCATION_SCHEMA.to_string()),
    );
    root.insert(
        "builder_tag".to_string(),
        Value::String(builder_tag.to_string()),
    );
    root.insert("payload".to_string(), normalized_payload.clone());
    root.insert("input_build_keys".to_string(), Value::Array(input_keys));

    let canonical = crate::json::canonical_json_bytes(&Value::Object(root))?;
    Ok(BuildKey::from_bytes(Sha256::digest(&canonical).into()))
}

/// Computes the stable reuse key for a normalized result invocation.
///
/// The key covers the builder tag, the normalized JSON payload, and the ordered
/// list of realized input object identities. Runtime code uses this key to find
/// a reusable result even when the current build key has not been seen before.
pub fn compute_reuse_key(
    builder_tag: &str,
    normalized_payload: &Value,
    inputs: &[ReuseInputIdentity],
) -> Result<ReuseKey, StoreError> {
    let input_values = inputs
        .iter()
        .map(|input| {
            let mut object = Map::new();
            object.insert(
                "object_hash".to_string(),
                Value::String(input.object_hash.to_string()),
            );
            Value::Object(object)
        })
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(RESULT_INVOCATION_SCHEMA.to_string()),
    );
    root.insert(
        "builder_tag".to_string(),
        Value::String(builder_tag.to_string()),
    );
    root.insert("payload".to_string(), normalized_payload.clone());
    root.insert("inputs".to_string(), Value::Array(input_values));

    let canonical = crate::json::canonical_json_bytes(&Value::Object(root))?;
    Ok(ReuseKey::from_bytes(Sha256::digest(&canonical).into()))
}

/// Computes the stable result id for an imported object hash.
///
/// The result id is the key under which the result record is stored. It is
/// derived only from the output object's [`ObjectHash`], so equivalent output
/// objects share the same result record.
pub fn compute_result_id(object_hash: ObjectHash) -> ResultId {
    let canonical = format!(
        "{{\"object_hash\":\"{}\",\"schema\":\"bobr-result-id-v2\"}}",
        object_hash
    );
    ResultId::from_bytes(Sha256::digest(canonical.as_bytes()).into())
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

fn parse_hex_32(value: &str) -> Result<[u8; 32], ParseIdentityError> {
    if value.len() != 64 {
        return Err(ParseIdentityError::InvalidLength);
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ParseIdentityError::InvalidHex);
    }

    let mut bytes = [0u8; 32];
    for (idx, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let hi = decode_nibble(chunk[0]).ok_or(ParseIdentityError::InvalidHex)?;
        let lo = decode_nibble(chunk[1]).ok_or(ParseIdentityError::InvalidHex)?;
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
