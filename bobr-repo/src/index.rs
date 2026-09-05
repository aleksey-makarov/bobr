//! Generic fixed-width mapping indexes and content lists.

use crate::{MAX_METADATA_BYTES, MetadataHash, RepositoryError};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use bobr_store::fs_tree::FsFileHash;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::path::Path;

/// One 32-byte key usable by a repository metadata table.
pub trait FixedHashKey: Copy + Eq + Ord {
    /// Constructs the key from its raw representation.
    fn from_raw_bytes(bytes: [u8; 32]) -> Self;
    /// Returns the raw representation.
    fn raw_bytes(&self) -> &[u8; 32];
}

macro_rules! impl_fixed_hash_key {
    ($type:ty) => {
        impl FixedHashKey for $type {
            fn from_raw_bytes(bytes: [u8; 32]) -> Self {
                Self::from_bytes(bytes)
            }

            fn raw_bytes(&self) -> &[u8; 32] {
                self.as_bytes()
            }
        }
    };
}

impl_fixed_hash_key!(BuildKey);
impl_fixed_hash_key!(ReuseKey);
impl_fixed_hash_key!(ObjectHash);
impl_fixed_hash_key!(FsFileHash);

/// Validated immutable mapping from a key to ordered object candidates.
#[derive(Debug, Clone)]
pub struct MappingIndex<K> {
    bytes: Box<[u8]>,
    marker: PhantomData<fn() -> K>,
}

/// Validated immutable sorted set of content hashes.
#[derive(Debug, Clone)]
pub struct ContentList<H> {
    bytes: Box<[u8]>,
    marker: PhantomData<fn() -> H>,
}

/// Build-key mapping index.
pub type BuildIndex = MappingIndex<BuildKey>;
/// Reuse-key mapping index.
pub type ReuseIndex = MappingIndex<ReuseKey>;
/// Advertised ordinary-object set.
pub type ObjectList = ContentList<ObjectHash>;
/// Advertised filesystem-file set.
pub type FsFileList = ContentList<FsFileHash>;

/// Digest of one build-key index.
pub type BuildIndexHash = MetadataHash<BuildIndex>;
/// Digest of one reuse-key index.
pub type ReuseIndexHash = MetadataHash<ReuseIndex>;
/// Digest of one ordinary-object list.
pub type ObjectListHash = MetadataHash<ObjectList>;
/// Digest of one filesystem-file list.
pub type FsFileListHash = MetadataHash<FsFileList>;

impl<K: FixedHashKey> MappingIndex<K> {
    /// Parses and validates an index from exact bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, RepositoryError> {
        check_metadata_size(bytes.len())?;
        if !bytes.len().is_multiple_of(64) {
            return Err(RepositoryError::new(
                "mapping index size is not divisible by 64",
            ));
        }
        let mut previous_key: Option<[u8; 32]> = None;
        let mut hashes_for_key = HashSet::new();
        for record in bytes.chunks_exact(64) {
            let key: [u8; 32] = record[..32].try_into().expect("fixed record key");
            let hash: [u8; 32] = record[32..].try_into().expect("fixed record hash");
            if let Some(previous) = previous_key {
                if key < previous {
                    return Err(RepositoryError::new(
                        "mapping index keys are not in nondecreasing order",
                    ));
                }
                if key != previous {
                    hashes_for_key.clear();
                }
            }
            if !hashes_for_key.insert(hash) {
                return Err(RepositoryError::new(
                    "mapping index contains a duplicate key/object pair",
                ));
            }
            previous_key = Some(key);
        }
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            marker: PhantomData,
        })
    }

    /// Reads, hashes, and validates an index file.
    pub fn open(path: &Path, expected: MetadataHash<Self>) -> Result<Self, RepositoryError> {
        check_metadata_size_before_read(path)?;
        let bytes = std::fs::read(path)?;
        if MetadataHash::<Self>::digest(&bytes) != expected {
            return Err(RepositoryError::new(format!(
                "metadata hash mismatch while opening '{}'",
                path.display()
            )));
        }
        Self::from_bytes(bytes)
    }

    /// Serializes ordered records after validating key grouping and duplicate pairs.
    pub fn encode(records: &[(K, ObjectHash)]) -> Result<Vec<u8>, RepositoryError> {
        let mut bytes = Vec::with_capacity(records.len().saturating_mul(64));
        for (key, hash) in records {
            bytes.extend_from_slice(key.raw_bytes());
            bytes.extend_from_slice(hash.as_bytes());
        }
        Self::from_bytes(bytes.clone())?;
        Ok(bytes)
    }

    /// Returns ordered, distinct object candidates for `key`.
    pub fn candidates(&self, key: K) -> Vec<ObjectHash> {
        let records = self.bytes.len() / 64;
        let start = lower_bound(records, |index| {
            self.bytes[index * 64..index * 64 + 32].cmp(key.raw_bytes())
        });
        let mut candidates = Vec::new();
        for record in self.bytes[start * 64..].chunks_exact(64) {
            if record[..32] != key.raw_bytes()[..] {
                break;
            }
            let hash = ObjectHash::from_bytes(record[32..].try_into().expect("fixed object hash"));
            if !candidates.contains(&hash) {
                candidates.push(hash);
            }
        }
        candidates
    }

    /// Iterates over all mapping records in wire order.
    pub fn records(&self) -> impl Iterator<Item = (K, ObjectHash)> + '_ {
        self.bytes.chunks_exact(64).map(|record| {
            (
                K::from_raw_bytes(record[..32].try_into().expect("validated mapping key")),
                ObjectHash::from_bytes(
                    record[32..]
                        .try_into()
                        .expect("validated mapping object hash"),
                ),
            )
        })
    }

    /// Returns the exact immutable index bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl<H: FixedHashKey> ContentList<H> {
    /// Parses and validates a sorted unique content list.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, RepositoryError> {
        check_metadata_size(bytes.len())?;
        if !bytes.len().is_multiple_of(32) {
            return Err(RepositoryError::new(
                "content list size is not divisible by 32",
            ));
        }
        let mut previous: Option<[u8; 32]> = None;
        for raw in bytes.chunks_exact(32) {
            let current: [u8; 32] = raw.try_into().expect("fixed content hash");
            if previous.is_some_and(|previous| current <= previous) {
                return Err(RepositoryError::new(
                    "content list hashes are not strictly increasing",
                ));
            }
            previous = Some(current);
        }
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            marker: PhantomData,
        })
    }

    /// Reads, hashes, and validates a content-list file.
    pub fn open(path: &Path, expected: MetadataHash<Self>) -> Result<Self, RepositoryError> {
        check_metadata_size_before_read(path)?;
        let bytes = std::fs::read(path)?;
        if MetadataHash::<Self>::digest(&bytes) != expected {
            return Err(RepositoryError::new(format!(
                "metadata hash mismatch while opening '{}'",
                path.display()
            )));
        }
        Self::from_bytes(bytes)
    }

    /// Encodes hashes in canonical increasing order.
    pub fn encode(hashes: impl IntoIterator<Item = H>) -> Vec<u8> {
        let mut hashes = hashes.into_iter().collect::<Vec<_>>();
        hashes.sort_unstable();
        hashes.dedup();
        let mut bytes = Vec::with_capacity(hashes.len().saturating_mul(32));
        for hash in hashes {
            bytes.extend_from_slice(hash.raw_bytes());
        }
        bytes
    }

    /// Returns whether this authoritative list advertises `hash`.
    pub fn contains(&self, hash: H) -> bool {
        let records = self.bytes.len() / 32;
        let index = lower_bound(records, |index| {
            self.bytes[index * 32..index * 32 + 32].cmp(hash.raw_bytes())
        });
        index < records && self.bytes[index * 32..index * 32 + 32] == hash.raw_bytes()[..]
    }

    /// Iterates over all hashes in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = H> + '_ {
        self.bytes
            .chunks_exact(32)
            .map(|raw| H::from_raw_bytes(raw.try_into().expect("validated fixed content hash")))
    }

    /// Returns the exact immutable list bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn check_metadata_size(size: usize) -> Result<(), RepositoryError> {
    if size as u64 > MAX_METADATA_BYTES {
        return Err(RepositoryError::new(format!(
            "immutable metadata exceeds the format limit of {MAX_METADATA_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_metadata_size_before_read(path: &Path) -> Result<(), RepositoryError> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_METADATA_BYTES {
        return Err(RepositoryError::new(format!(
            "immutable metadata exceeds the format limit of {MAX_METADATA_BYTES} bytes"
        )));
    }
    Ok(())
}

fn lower_bound(mut len: usize, mut compare: impl FnMut(usize) -> std::cmp::Ordering) -> usize {
    let mut base = 0;
    while len > 0 {
        let half = len / 2;
        let middle = base + half;
        if compare(middle).is_lt() {
            base = middle + 1;
            len -= half + 1;
        } else {
            len = half;
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(byte: u8) -> BuildKey {
        BuildKey::from_bytes([byte; 32])
    }

    fn object(byte: u8) -> ObjectHash {
        ObjectHash::from_bytes([byte; 32])
    }

    #[test]
    fn mapping_index_preserves_candidate_priority() {
        let records = vec![
            (build(1), object(8)),
            (build(1), object(7)),
            (build(2), object(6)),
        ];
        let bytes = BuildIndex::encode(&records).unwrap();
        let index = BuildIndex::from_bytes(bytes).unwrap();
        assert_eq!(index.candidates(build(1)), vec![object(8), object(7)]);
        assert!(index.candidates(build(3)).is_empty());
    }

    #[test]
    fn mapping_index_rejects_unsorted_and_duplicate_pairs() {
        assert!(BuildIndex::encode(&[(build(2), object(1)), (build(1), object(2))]).is_err());
        assert!(BuildIndex::encode(&[(build(1), object(2)), (build(1), object(2))]).is_err());
        assert!(
            BuildIndex::encode(&[
                (build(1), object(2)),
                (build(1), object(3)),
                (build(1), object(2)),
            ])
            .is_err()
        );
    }

    #[test]
    fn content_list_is_canonical_and_searchable() {
        let bytes = ObjectList::encode([object(3), object(1), object(3), object(2)]);
        let list = ObjectList::from_bytes(bytes).unwrap();
        assert_eq!(
            list.iter().collect::<Vec<_>>(),
            vec![object(1), object(2), object(3)]
        );
        assert!(list.contains(object(2)));
        assert!(!list.contains(object(4)));
    }

    #[test]
    fn typed_metadata_hash_roundtrips() {
        let hash = BuildIndexHash::digest(b"index");
        assert_eq!(hash.to_string().parse::<BuildIndexHash>().unwrap(), hash);
        assert!(
            hash.to_string()
                .to_uppercase()
                .parse::<BuildIndexHash>()
                .is_err()
        );
    }
}
