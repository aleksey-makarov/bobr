//! Authenticated repository master format.

use crate::{
    BuildIndexHash, Decoder, Encoder, FsFileListHash, MAX_MASTER_BYTES, ObjectListHash,
    REPOSITORY_FORMAT, RepositoryError, ReuseIndexHash,
};
use coset::{Algorithm, CoseSign1, CoseSign1Builder, HeaderBuilder, TaggedCborSerializable, iana};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::collections::BTreeMap;
use url::Url;

/// COSE protected-header content type for a repository master.
pub const MASTER_CONTENT_TYPE: &str = "application/vnd.bobr.repository-master+cbor";

/// One immutable slot state authenticated by a master.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// Monotonic identity of this immutable slot state.
    pub serial: u64,
    /// Digest of the build-key mapping index.
    pub build: BuildIndexHash,
    /// Digest of the reuse-key mapping index.
    pub reuse: ReuseIndexHash,
    /// Digest of the authoritative ordinary-object list.
    pub object_list: ObjectListHash,
    /// Digest of the authoritative filesystem-file list.
    pub file_list: FsFileListHash,
    /// Retirement deadline, or `None` while the slot is current.
    pub retain_until: Option<u64>,
}

/// Verified logical payload embedded in `/master`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Master {
    data_base_url: Url,
    slots: Vec<Slot>,
}

/// An Ed25519 public-key set pinned by opaque COSE key identifier.
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
    keys: BTreeMap<Vec<u8>, VerifyingKey>,
}

/// Result of verifying an authenticated master.
#[derive(Debug, Clone)]
pub struct VerifiedMaster {
    /// Authenticated logical payload.
    pub master: Master,
    /// Pinned key identifier which authenticated the response.
    pub key_id: Vec<u8>,
    /// Exact tagged COSE representation received from the origin.
    pub signed_bytes: Vec<u8>,
}

impl Master {
    /// Constructs and validates a logical master payload.
    pub fn new(data_base_url: Url, slots: Vec<Slot>) -> Result<Self, RepositoryError> {
        let master = Self {
            data_base_url,
            slots,
        };
        master.validate()?;
        Ok(master)
    }

    /// Returns the absolute immutable-data base URL.
    pub fn data_base_url(&self) -> &Url {
        &self.data_base_url
    }

    /// Returns all current and retained states in increasing serial order.
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// Iterates over current slots from newest to oldest.
    pub fn current_slots_newest_first(&self) -> impl Iterator<Item = &Slot> {
        self.slots
            .iter()
            .rev()
            .filter(|slot| slot.retain_until.is_none())
    }

    /// Returns the active current slot.
    pub fn active_slot(&self) -> &Slot {
        self.current_slots_newest_first()
            .next()
            .expect("validated master has a current slot")
    }

    /// Encodes the deterministic CBOR payload embedded in COSE.
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.map(3);
        encoder.text("slots");
        encoder.array(self.slots.len() as u64);
        for slot in &self.slots {
            encode_slot(&mut encoder, slot);
        }
        encoder.text("data_base_url");
        encoder.text(self.data_base_url.as_str());
        encoder.text("repository_format");
        encoder.uint(REPOSITORY_FORMAT);
        encoder.finish()
    }

    /// Decodes and validates a deterministic CBOR payload.
    pub fn decode_payload(bytes: &[u8]) -> Result<Self, RepositoryError> {
        let mut decoder = Decoder::new(bytes);
        require_length(decoder.map()?, 3, "master map")?;
        require_text(&mut decoder, "slots")?;
        let slot_count = decoder.array()?;
        let slot_count = usize::try_from(slot_count)
            .map_err(|_| RepositoryError::new("slot count is too large"))?;
        let mut slots = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            slots.push(decode_slot(&mut decoder)?);
        }
        require_text(&mut decoder, "data_base_url")?;
        let data_base_url = Url::parse(decoder.text()?)
            .map_err(|error| RepositoryError::new(format!("invalid data_base_url: {error}")))?;
        require_text(&mut decoder, "repository_format")?;
        let repository_format = decoder.uint()?;
        decoder.finish()?;
        if repository_format != REPOSITORY_FORMAT {
            return Err(RepositoryError::new(format!(
                "unsupported repository format {repository_format}"
            )));
        }
        Self::new(data_base_url, slots)
    }

    /// Signs this payload as a tagged deterministic `COSE_Sign1` object.
    pub fn sign(&self, key_id: &[u8], key: &SigningKey) -> Result<Vec<u8>, RepositoryError> {
        if key_id.is_empty() {
            return Err(RepositoryError::new("COSE kid must not be empty"));
        }
        let protected = HeaderBuilder::new()
            .algorithm(iana::Algorithm::EdDSA)
            .content_type(MASTER_CONTENT_TYPE.to_owned())
            .key_id(key_id.to_vec())
            .build();
        let cose = CoseSign1Builder::new()
            .protected(protected)
            .payload(self.encode_payload())
            .create_signature(&[], |data| key.sign(data).to_bytes().to_vec())
            .build();
        let bytes = cose.to_tagged_vec().map_err(|error| {
            RepositoryError::new(format!("failed to encode COSE_Sign1: {error}"))
        })?;
        if bytes.len() as u64 > MAX_MASTER_BYTES {
            return Err(RepositoryError::new(
                "encoded master exceeds the format limit",
            ));
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        validate_data_base_url(&self.data_base_url)?;
        if self.slots.is_empty() {
            return Err(RepositoryError::new(
                "master must contain at least one slot",
            ));
        }
        let mut previous = None;
        let mut has_current = false;
        for slot in &self.slots {
            if previous.is_some_and(|previous| slot.serial <= previous) {
                return Err(RepositoryError::new(
                    "slot serials must be strictly increasing",
                ));
            }
            previous = Some(slot.serial);
            has_current |= slot.retain_until.is_none();
        }
        if !has_current {
            return Err(RepositoryError::new(
                "master must contain at least one current slot",
            ));
        }
        Ok(())
    }
}

impl TrustedKeys {
    /// Creates a pinned key set and rejects duplicate or empty identifiers.
    pub fn new(
        keys: impl IntoIterator<Item = (Vec<u8>, VerifyingKey)>,
    ) -> Result<Self, RepositoryError> {
        let mut trusted = Self::default();
        for (key_id, key) in keys {
            trusted.insert(key_id, key)?;
        }
        Ok(trusted)
    }

    /// Adds one key under its exact opaque identifier.
    pub fn insert(&mut self, key_id: Vec<u8>, key: VerifyingKey) -> Result<(), RepositoryError> {
        if key_id.is_empty() {
            return Err(RepositoryError::new("pinned key id must not be empty"));
        }
        if self.keys.insert(key_id, key).is_some() {
            return Err(RepositoryError::new("duplicate pinned key id"));
        }
        Ok(())
    }

    /// Authenticates and decodes a tagged repository master.
    pub fn verify(&self, bytes: &[u8]) -> Result<VerifiedMaster, RepositoryError> {
        if bytes.len() as u64 > MAX_MASTER_BYTES {
            return Err(RepositoryError::new(
                "encoded master exceeds the format limit",
            ));
        }
        let cose = CoseSign1::from_tagged_slice(bytes)
            .map_err(|error| RepositoryError::new(format!("invalid COSE_Sign1: {error}")))?;
        validate_cose_headers(&cose)?;
        let key_id = cose.protected.header.key_id.clone();
        let key = self
            .keys
            .get(&key_id)
            .ok_or_else(|| RepositoryError::new("master uses an untrusted COSE kid"))?;
        cose.verify_signature(&[], |signature, data| {
            let signature = Signature::from_slice(signature)?;
            key.verify(data, &signature)
        })
        .map_err(|error| RepositoryError::new(format!("invalid master signature: {error}")))?;
        let payload = cose
            .payload
            .as_deref()
            .ok_or_else(|| RepositoryError::new("detached master payload is forbidden"))?;
        let master = Master::decode_payload(payload)?;

        let mut canonical = cose.clone();
        canonical.protected.original_data = None;
        let canonical = canonical.to_tagged_vec().map_err(|error| {
            RepositoryError::new(format!(
                "failed to check deterministic COSE encoding: {error}"
            ))
        })?;
        if canonical != bytes {
            return Err(RepositoryError::new(
                "master COSE object does not use deterministic CBOR",
            ));
        }
        Ok(VerifiedMaster {
            master,
            key_id,
            signed_bytes: bytes.to_vec(),
        })
    }
}

fn validate_cose_headers(cose: &CoseSign1) -> Result<(), RepositoryError> {
    let protected = &cose.protected.header;
    if protected.alg != Some(Algorithm::Assigned(iana::Algorithm::EdDSA)) {
        return Err(RepositoryError::new("master COSE algorithm is not EdDSA"));
    }
    if protected.content_type != Some(coset::ContentType::Text(MASTER_CONTENT_TYPE.to_owned())) {
        return Err(RepositoryError::new("invalid master COSE content type"));
    }
    if protected.key_id.is_empty() {
        return Err(RepositoryError::new("master COSE kid is missing"));
    }
    if !protected.crit.is_empty()
        || !protected.iv.is_empty()
        || !protected.partial_iv.is_empty()
        || !protected.counter_signatures.is_empty()
        || !protected.rest.is_empty()
    {
        return Err(RepositoryError::new("unknown protected COSE header"));
    }
    if !cose.unprotected.is_empty() {
        return Err(RepositoryError::new(
            "unprotected COSE headers must be empty",
        ));
    }
    if cose.payload.is_none() {
        return Err(RepositoryError::new("detached master payload is forbidden"));
    }
    if cose.signature.len() != 64 {
        return Err(RepositoryError::new(
            "Ed25519 signature must contain 64 bytes",
        ));
    }
    Ok(())
}

fn validate_data_base_url(url: &Url) -> Result<(), RepositoryError> {
    if url.scheme() != "https"
        || url.cannot_be_a_base()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err(RepositoryError::new(
            "data_base_url must be an absolute HTTPS base URL ending in '/'",
        ));
    }
    Ok(())
}

fn encode_slot(encoder: &mut Encoder, slot: &Slot) {
    encoder.map(6);
    encoder.text("build");
    encoder.bytes(slot.build.as_bytes());
    encoder.text("reuse");
    encoder.bytes(slot.reuse.as_bytes());
    encoder.text("serial");
    encoder.uint(slot.serial);
    encoder.text("file_list");
    encoder.bytes(slot.file_list.as_bytes());
    encoder.text("object_list");
    encoder.bytes(slot.object_list.as_bytes());
    encoder.text("retain_until");
    match slot.retain_until {
        Some(value) => encoder.uint(value),
        None => encoder.null(),
    }
}

fn decode_slot(decoder: &mut Decoder<'_>) -> Result<Slot, RepositoryError> {
    require_length(decoder.map()?, 6, "slot map")?;
    require_text(decoder, "build")?;
    let build = BuildIndexHash::from_bytes(require_hash(decoder.bytes()?)?);
    require_text(decoder, "reuse")?;
    let reuse = ReuseIndexHash::from_bytes(require_hash(decoder.bytes()?)?);
    require_text(decoder, "serial")?;
    let serial = decoder.uint()?;
    require_text(decoder, "file_list")?;
    let file_list = FsFileListHash::from_bytes(require_hash(decoder.bytes()?)?);
    require_text(decoder, "object_list")?;
    let object_list = ObjectListHash::from_bytes(require_hash(decoder.bytes()?)?);
    require_text(decoder, "retain_until")?;
    let retain_until = decoder.null_or_uint()?;
    Ok(Slot {
        serial,
        build,
        reuse,
        object_list,
        file_list,
        retain_until,
    })
}

fn require_hash(bytes: &[u8]) -> Result<[u8; 32], RepositoryError> {
    bytes
        .try_into()
        .map_err(|_| RepositoryError::new("master metadata digest must contain 32 bytes"))
}

fn require_text(decoder: &mut Decoder<'_>, expected: &str) -> Result<(), RepositoryError> {
    if decoder.text()? == expected {
        Ok(())
    } else {
        Err(RepositoryError::new(format!(
            "unexpected or incorrectly ordered CBOR map key; expected '{expected}'"
        )))
    }
}

fn require_length(actual: u64, expected: u64, what: &str) -> Result<(), RepositoryError> {
    if actual == expected {
        Ok(())
    } else {
        Err(RepositoryError::new(format!(
            "{what} must contain exactly {expected} entries"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master() -> Master {
        let digest = [7; 32];
        Master::new(
            Url::parse("https://objects.example/repository/").unwrap(),
            vec![Slot {
                serial: 41,
                build: BuildIndexHash::from_bytes(digest),
                reuse: ReuseIndexHash::from_bytes(digest),
                object_list: ObjectListHash::from_bytes(digest),
                file_list: FsFileListHash::from_bytes(digest),
                retain_until: None,
            }],
        )
        .unwrap()
    }

    #[test]
    fn deterministic_payload_roundtrips() {
        let master = master();
        let bytes = master.encode_payload();
        assert_eq!(Master::decode_payload(&bytes).unwrap(), master);
        assert_eq!(bytes.first(), Some(&0xa3));

        let mut noncanonical = vec![0xb8, 3];
        noncanonical.extend_from_slice(&bytes[1..]);
        assert!(Master::decode_payload(&noncanonical).is_err());
    }

    #[test]
    fn signed_master_roundtrips_and_authenticates() {
        let signing = SigningKey::from_bytes(&[3; 32]);
        let bytes = master().sign(b"release-1", &signing).unwrap();
        assert_eq!(bytes.first(), Some(&0xd2));
        let keys = TrustedKeys::new([(b"release-1".to_vec(), signing.verifying_key())]).unwrap();
        let verified = keys.verify(&bytes).unwrap();
        assert_eq!(verified.master, master());
        assert_eq!(verified.key_id, b"release-1");

        let mut tampered = bytes;
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(keys.verify(&tampered).is_err());
    }

    #[test]
    fn master_rejects_bad_urls_and_slot_order() {
        let slot = master().slots[0].clone();
        assert!(
            Master::new(
                Url::parse("http://example/repo/").unwrap(),
                vec![slot.clone()]
            )
            .is_err()
        );
        let mut older = slot.clone();
        older.serial = slot.serial + 1;
        assert!(
            Master::new(
                Url::parse("https://example/repo/").unwrap(),
                vec![older, slot]
            )
            .is_err()
        );
    }

    #[test]
    fn untrusted_kid_is_rejected() {
        let signing = SigningKey::from_bytes(&[3; 32]);
        let bytes = master().sign(b"unknown", &signing).unwrap();
        assert!(TrustedKeys::default().verify(&bytes).is_err());
    }
}
