//! Stable build identity computation.
//!
//! This module computes deterministic identifiers from normalized semantic
//! inputs: requested build graph keys, reuse keys for realized input objects,
//! and object hashes for normalized filesystem objects.

use fsobj_hash::define_hex_hash_type;
pub use fsobj_hash::{ObjectHash, ParseHexHashError};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

const INVOCATION_SCHEMA: &str = "bobr-build-invocation-v5";
const REUSE_INVOCATION_SCHEMA: &str = "bobr-build-reuse-invocation-v5";

/// Manually-bumped version of the bobr build subsystem's key semantics.
///
/// It is folded into the per-builder version token that enters every build and
/// reuse key (see the builder-subject code), so bumping it invalidates all
/// keys. Bump it when a core change alters build outputs for an otherwise
/// unchanged request. (The input-name materialization rule — an input is
/// materialized into an fs-tree root iff its name begins with `_` — bumped this
/// from "1" to "2".)
pub const BOBR_BUILD_CORE_VERSION: &str = "2";

define_hex_hash_type! {
    /// Stable key for a planned build graph node.
    ///
    /// Builder build keys are SHA-256 digests produced by
    /// [`compute_build_key`]. Source build keys are produced by
    /// [`BuildKey::from_object_hash`] and have the same textual representation
    /// as the declared source object hash.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct BuildKey;
}

impl BuildKey {
    /// Returns the source build key for an object hash.
    ///
    /// Source nodes have no planned inputs. Their graph/build key is the
    /// declared object hash reinterpreted in the build-key domain, so the
    /// textual representation is unchanged.
    pub fn from_object_hash(object_hash: ObjectHash) -> Self {
        Self::from_bytes(*object_hash.as_bytes())
    }

    /// Returns a short human-readable prefix of the key.
    pub fn short(self) -> String {
        self.to_string().chars().take(12).collect()
    }
}

define_hex_hash_type! {
    /// Stable key used to reuse an existing object across equivalent inputs.
    ///
    /// A reuse key is produced by [`compute_reuse_key`] from the builder tag,
    /// normalized payload, and realized input object hashes.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct ReuseKey;
}

define_hex_hash_type! {
    /// Deterministic per-build seed for builders that need a "random-looking"
    /// but reproducible value (e.g. a filesystem UUID).
    ///
    /// It is derived from the [`ReuseKey`], which already digests the builder
    /// tag, version, normalized config, and the realized input object hashes.
    /// Deriving from the reuse key — rather than the build key — is what keeps
    /// the seed consistent with content-addressed reuse: two graphs that reach
    /// the same inputs share one reuse key, hence one seed, hence identical
    /// output. The seed is domain-separated so it is never equal to the reuse
    /// key itself.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters. This is deterministic, not secret and not real entropy; do
    /// not use it for anything security-sensitive.
    pub struct BuildSeed;
}

impl BuildSeed {
    const DOMAIN: &'static [u8] = b"bobr-build-seed\0";

    /// The all-zero seed, used by builders and tests that do not need one.
    pub const ZERO: BuildSeed = BuildSeed([0u8; 32]);

    /// Derives the seed for a build from its reuse key.
    pub fn from_reuse_key(reuse_key: &ReuseKey) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(Self::DOMAIN);
        hasher.update(reuse_key.as_bytes());
        Self::from_bytes(hasher.finalize().into())
    }
}

/// Error returned while computing build identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityError {
    message: String,
}

impl IdentityError {
    fn json(error: serde_json::Error) -> Self {
        Self {
            message: format!("failed to serialize canonical identity JSON: {error}"),
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IdentityError {}

/// Digest of a builder's normalized config, embedded in build and reuse keys.
///
/// A key is a fingerprint built from fixed-size digests of its parts. The config
/// enters as this digest rather than as an inlined JSON sub-document, so the key
/// root stays small and uniform regardless of how large or nested the config is,
/// and the config is canonicalized in exactly one place. Unlike [`BuildKey`] /
/// [`ReuseKey`] this is a purely internal digest: it has no textual identity and
/// is never stored or looked up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConfigDigest([u8; 32]);

impl ConfigDigest {
    /// Digests a normalized config value: canonical JSON, then SHA-256.
    pub fn of(config: &Value) -> Result<Self, IdentityError> {
        let canonical = canonical_json_bytes(config)?;
        Ok(Self(Sha256::digest(&canonical).into()))
    }

    fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(b"0123456789abcdef"[(byte >> 4) as usize] as char);
            out.push(b"0123456789abcdef"[(byte & 0x0f) as usize] as char);
        }
        out
    }
}

/// A dependency hash that can be embedded in a key's `inputs` list. Lets the
/// build key (dependency `build_key`s) and the reuse key (dependency
/// `object_hash`es) share one key-encoding routine.
trait KeyInputHash {
    /// JSON field name under which the hash is recorded.
    const FIELD: &'static str;
    /// Textual (hex) encoding of the hash.
    fn encode(&self) -> String;
}

impl KeyInputHash for BuildKey {
    const FIELD: &'static str = "build_key";
    fn encode(&self) -> String {
        self.to_string()
    }
}

impl KeyInputHash for ObjectHash {
    const FIELD: &'static str = "object_hash";
    fn encode(&self) -> String {
        self.to_string()
    }
}

/// Encodes a build invocation into a canonical JSON fingerprint and hashes it.
///
/// The root records the schema, builder tag and version token, the config
/// digest, and the dependency hashes **keyed by input slot name** (both the slot
/// name and the hash enter the fingerprint, so wiring the same objects into
/// differently named slots yields distinct keys). The `BTreeMap` makes the
/// encoding order-independent and deterministic.
fn compute_key<H: KeyInputHash>(
    schema: &str,
    builder_tag: &str,
    builder_version: &str,
    config: ConfigDigest,
    inputs: &BTreeMap<String, H>,
) -> Result<[u8; 32], IdentityError> {
    let input_values = inputs
        .iter()
        .map(|(name, hash)| {
            let mut input = Map::new();
            input.insert("name".to_string(), Value::String(name.clone()));
            input.insert(H::FIELD.to_string(), Value::String(hash.encode()));
            Value::Object(input)
        })
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert("schema".to_string(), Value::String(schema.to_string()));
    root.insert(
        "builder_tag".to_string(),
        Value::String(builder_tag.to_string()),
    );
    root.insert(
        "builder_version".to_string(),
        Value::String(builder_version.to_string()),
    );
    root.insert("config".to_string(), Value::String(config.to_hex()));
    root.insert("inputs".to_string(), Value::Array(input_values));

    let canonical = canonical_json_bytes(&Value::Object(root))?;
    Ok(Sha256::digest(&canonical).into())
}

/// Computes the stable build key from the builder tag, version token, config
/// digest, and the direct dependency build keys.
///
/// `builder_version` is the builder's implementation-version token: it captures
/// the identity of the build logic itself, so bumping it invalidates this
/// builder's cached outputs.
pub fn compute_build_key(
    builder_tag: &str,
    builder_version: &str,
    config: ConfigDigest,
    inputs: &BTreeMap<String, BuildKey>,
) -> Result<BuildKey, IdentityError> {
    Ok(BuildKey::from_bytes(compute_key(
        INVOCATION_SCHEMA,
        builder_tag,
        builder_version,
        config,
        inputs,
    )?))
}

/// Computes the stable reuse key from the builder tag, version token, config
/// digest, and the realized dependency object hashes. Runtime code uses it to
/// find a reusable object even when the current build key has not been seen.
pub fn compute_reuse_key(
    builder_tag: &str,
    builder_version: &str,
    config: ConfigDigest,
    inputs: &BTreeMap<String, ObjectHash>,
) -> Result<ReuseKey, IdentityError> {
    Ok(ReuseKey::from_bytes(compute_key(
        REUSE_INVOCATION_SCHEMA,
        builder_tag,
        builder_version,
        config,
        inputs,
    )?))
}

fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, IdentityError> {
    let mut out = Vec::new();
    write_canonical_json(value, &mut out)?;
    Ok(out)
}

fn write_canonical_json(value: &Value, out: &mut Vec<u8>) -> Result<(), IdentityError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_writer(out, value).map_err(IdentityError::json)
        }
        Value::Array(items) => {
            out.push(b'[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                write_canonical_json(item, out)?;
            }
            out.push(b']');
            Ok(())
        }
        Value::Object(object) => {
            out.push(b'{');
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            for (idx, key) in keys.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).map_err(IdentityError::json)?;
                out.push(b':');
                write_canonical_json(&object[*key], out)?;
            }
            out.push(b'}');
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    const HEX_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEX_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn build_seed_is_deterministic_domain_separated_and_distinct() {
        let key_a = ReuseKey::from_str(HEX_A).unwrap();
        let key_b = ReuseKey::from_str(HEX_B).unwrap();

        // Deterministic: same reuse key -> same seed.
        assert_eq!(
            BuildSeed::from_reuse_key(&key_a),
            BuildSeed::from_reuse_key(&key_a)
        );
        // Distinct reuse keys -> distinct seeds.
        assert_ne!(
            BuildSeed::from_reuse_key(&key_a),
            BuildSeed::from_reuse_key(&key_b)
        );
        // Domain-separated: the seed is never just the reuse key bytes.
        assert_ne!(
            BuildSeed::from_reuse_key(&key_a).as_bytes(),
            key_a.as_bytes()
        );
        // Hex is 64 lowercase chars.
        let hex = BuildSeed::from_reuse_key(&key_a).to_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn build_key_distinguishes_inputs_by_slot_name() {
        let key = BuildKey::from_str(HEX_A).unwrap();
        let under_a = compute_build_key(
            "T",
            "1",
            ConfigDigest::of(&json!({})).unwrap(),
            &BTreeMap::from([("a".to_string(), key)]),
        )
        .unwrap();
        let under_b = compute_build_key(
            "T",
            "1",
            ConfigDigest::of(&json!({})).unwrap(),
            &BTreeMap::from([("b".to_string(), key)]),
        )
        .unwrap();
        assert_ne!(under_a, under_b);
    }

    #[test]
    fn build_key_distinguishes_swapped_inputs() {
        let k1 = BuildKey::from_str(HEX_A).unwrap();
        let k2 = BuildKey::from_str(HEX_B).unwrap();
        let forward = compute_build_key(
            "T",
            "1",
            ConfigDigest::of(&json!({})).unwrap(),
            &BTreeMap::from([("a".to_string(), k1), ("b".to_string(), k2)]),
        )
        .unwrap();
        let swapped = compute_build_key(
            "T",
            "1",
            ConfigDigest::of(&json!({})).unwrap(),
            &BTreeMap::from([("a".to_string(), k2), ("b".to_string(), k1)]),
        )
        .unwrap();
        assert_ne!(forward, swapped);
    }

    #[test]
    fn reuse_key_distinguishes_inputs_by_slot_name() {
        let hash = ObjectHash::from_str(HEX_A).unwrap();
        let under_a = compute_reuse_key(
            "T",
            "1",
            ConfigDigest::of(&json!({})).unwrap(),
            &BTreeMap::from([("a".to_string(), hash)]),
        )
        .unwrap();
        let under_b = compute_reuse_key(
            "T",
            "1",
            ConfigDigest::of(&json!({})).unwrap(),
            &BTreeMap::from([("b".to_string(), hash)]),
        )
        .unwrap();
        assert_ne!(under_a, under_b);
    }

    #[test]
    fn builder_version_changes_both_keys() {
        let payload = ConfigDigest::of(&json!({})).unwrap();
        let build_inputs = BTreeMap::from([("a".to_string(), BuildKey::from_str(HEX_A).unwrap())]);
        let reuse_inputs =
            BTreeMap::from([("a".to_string(), ObjectHash::from_str(HEX_A).unwrap())]);

        assert_ne!(
            compute_build_key("T", "1", payload, &build_inputs).unwrap(),
            compute_build_key("T", "2", payload, &build_inputs).unwrap()
        );
        assert_ne!(
            compute_reuse_key("T", "1", payload, &reuse_inputs).unwrap(),
            compute_reuse_key("T", "2", payload, &reuse_inputs).unwrap()
        );
    }

    #[test]
    fn config_digest_changes_the_build_key() {
        let inputs = BTreeMap::from([("a".to_string(), BuildKey::from_str(HEX_A).unwrap())]);
        let empty = ConfigDigest::of(&json!({})).unwrap();
        let nonempty = ConfigDigest::of(&json!({ "flag": true })).unwrap();
        assert_ne!(
            compute_build_key("T", "1", empty, &inputs).unwrap(),
            compute_build_key("T", "1", nonempty, &inputs).unwrap()
        );
    }
}
