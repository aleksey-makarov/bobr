//! Pure publication state machine shared by repository writer frontends.

use crate::{
    BuildIndex, BuildIndexHash, FsFileList, FsFileListHash, Master, ObjectList, ObjectListHash,
    RepositoryError, ReuseIndex, ReuseIndexHash, Slot,
};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use bobr_store::fs_tree::FsFileHash;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use url::Url;

/// Complete independently closed logical contents of one slot state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotContents {
    builds: BTreeMap<BuildKey, Vec<ObjectHash>>,
    reuses: BTreeMap<ReuseKey, Vec<ObjectHash>>,
    objects: BTreeSet<ObjectHash>,
    files: BTreeSet<FsFileHash>,
}

/// One current or retained slot and its verified immutable metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationSlot {
    /// Monotonic immutable state identity.
    pub serial: u64,
    /// Retirement deadline, or `None` while this slot is current.
    pub retain_until: Option<u64>,
    /// Closed mappings and content lists represented by the slot.
    pub contents: SlotContents,
}

/// Complete durable publication state recoverable from a verified repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationState {
    data_base_url: Url,
    slots: Vec<PublicationSlot>,
}

/// Explicit operation selected by a repository writer frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationMode {
    /// Replace the active state with its union with new store contents.
    AppendToActive {
        /// Deadline assigned to the replaced active state.
        retain_until: u64,
    },
    /// Retire the oldest current state and start a fresh active state.
    Rotate {
        /// Deadline assigned to the retired oldest current state.
        retain_until: u64,
    },
}

/// Exact immutable metadata produced for one master publication.
#[derive(Debug, Clone)]
pub struct PublicationMetadata {
    /// Logical unsigned master ready for COSE signing.
    pub master: Master,
    /// Build indexes keyed by their content digest.
    pub builds: BTreeMap<BuildIndexHash, Vec<u8>>,
    /// Reuse indexes keyed by their content digest.
    pub reuses: BTreeMap<ReuseIndexHash, Vec<u8>>,
    /// Object lists keyed by their content digest.
    pub object_lists: BTreeMap<ObjectListHash, Vec<u8>>,
    /// Filesystem-file lists keyed by their content digest.
    pub file_lists: BTreeMap<FsFileListHash, Vec<u8>>,
}

impl SlotContents {
    /// Constructs a slot from wire-order mappings and advertised content sets.
    pub fn new(
        builds: impl IntoIterator<Item = (BuildKey, ObjectHash)>,
        reuses: impl IntoIterator<Item = (ReuseKey, ObjectHash)>,
        objects: impl IntoIterator<Item = ObjectHash>,
        files: impl IntoIterator<Item = FsFileHash>,
    ) -> Result<Self, RepositoryError> {
        let contents = Self {
            builds: collect_candidates(builds),
            reuses: collect_candidates(reuses),
            objects: objects.into_iter().collect(),
            files: files.into_iter().collect(),
        };
        contents.validate_closure()?;
        Ok(contents)
    }

    /// Returns build mappings in canonical key and candidate-priority order.
    pub fn build_records(&self) -> Vec<(BuildKey, ObjectHash)> {
        flatten_candidates(&self.builds)
    }

    /// Returns reuse mappings in canonical key and candidate-priority order.
    pub fn reuse_records(&self) -> Vec<(ReuseKey, ObjectHash)> {
        flatten_candidates(&self.reuses)
    }

    /// Iterates over the authoritative ordinary-object set.
    pub fn objects(&self) -> impl Iterator<Item = ObjectHash> + '_ {
        self.objects.iter().copied()
    }

    /// Iterates over the authoritative filesystem-file set.
    pub fn files(&self) -> impl Iterator<Item = FsFileHash> + '_ {
        self.files.iter().copied()
    }

    /// Adds newer mappings and content ahead of existing candidates.
    pub fn merge_newer(&mut self, newer: &Self) -> Result<(), RepositoryError> {
        merge_candidates(&mut self.builds, &newer.builds);
        merge_candidates(&mut self.reuses, &newer.reuses);
        self.objects.extend(newer.objects.iter().copied());
        self.files.extend(newer.files.iter().copied());
        self.validate_closure()
    }

    fn validate_closure(&self) -> Result<(), RepositoryError> {
        for object in self.builds.values().chain(self.reuses.values()).flatten() {
            if !self.objects.contains(object) {
                return Err(RepositoryError::new(format!(
                    "slot mapping references object '{object}' absent from its object list"
                )));
            }
        }
        Ok(())
    }
}

impl PublicationState {
    /// Initializes a repository with a caller-selected number of current slots.
    ///
    /// Empty sealed slots precede an active slot populated with `initial`.
    pub fn initialize(
        data_base_url: Url,
        current_slot_count: usize,
        initial: SlotContents,
    ) -> Result<Self, RepositoryError> {
        if current_slot_count == 0 {
            return Err(RepositoryError::new(
                "repository must have at least one current slot",
            ));
        }
        let mut slots = (0..current_slot_count)
            .map(|serial| PublicationSlot {
                serial: serial as u64,
                retain_until: None,
                contents: SlotContents::default(),
            })
            .collect::<Vec<_>>();
        slots.last_mut().expect("nonempty slots").contents = initial;
        Self::from_slots(data_base_url, slots)
    }

    /// Reconstructs publication state from authenticated, hash-verified slots.
    pub fn from_slots(
        data_base_url: Url,
        slots: Vec<PublicationSlot>,
    ) -> Result<Self, RepositoryError> {
        for slot in &slots {
            slot.contents.validate_closure()?;
        }
        let state = Self {
            data_base_url,
            slots,
        };
        state.metadata()?;
        Ok(state)
    }

    /// Reconstructs durable state from already hash-verified publication metadata.
    pub fn from_metadata(metadata: &PublicationMetadata) -> Result<Self, RepositoryError> {
        let mut slots = Vec::with_capacity(metadata.master.slots().len());
        for descriptor in metadata.master.slots() {
            let build_bytes = metadata.builds.get(&descriptor.build).ok_or_else(|| {
                RepositoryError::new(format!(
                    "publication metadata lacks build index '{}'",
                    descriptor.build
                ))
            })?;
            let reuse_bytes = metadata.reuses.get(&descriptor.reuse).ok_or_else(|| {
                RepositoryError::new(format!(
                    "publication metadata lacks reuse index '{}'",
                    descriptor.reuse
                ))
            })?;
            let object_bytes = metadata
                .object_lists
                .get(&descriptor.object_list)
                .ok_or_else(|| {
                    RepositoryError::new(format!(
                        "publication metadata lacks object list '{}'",
                        descriptor.object_list
                    ))
                })?;
            let file_bytes = metadata
                .file_lists
                .get(&descriptor.file_list)
                .ok_or_else(|| {
                    RepositoryError::new(format!(
                        "publication metadata lacks filesystem-file list '{}'",
                        descriptor.file_list
                    ))
                })?;
            if BuildIndexHash::digest(build_bytes) != descriptor.build
                || ReuseIndexHash::digest(reuse_bytes) != descriptor.reuse
                || ObjectListHash::digest(object_bytes) != descriptor.object_list
                || FsFileListHash::digest(file_bytes) != descriptor.file_list
            {
                return Err(RepositoryError::new(
                    "publication metadata digest does not match master",
                ));
            }
            let builds = BuildIndex::from_bytes(build_bytes.clone())?;
            let reuses = ReuseIndex::from_bytes(reuse_bytes.clone())?;
            let objects = ObjectList::from_bytes(object_bytes.clone())?;
            let files = FsFileList::from_bytes(file_bytes.clone())?;
            slots.push(PublicationSlot {
                serial: descriptor.serial,
                retain_until: descriptor.retain_until,
                contents: SlotContents::new(
                    builds.records(),
                    reuses.records(),
                    objects.iter(),
                    files.iter(),
                )?,
            });
        }
        Self::from_slots(metadata.master.data_base_url().clone(), slots)
    }

    /// Applies exactly the append or rotation operation selected by the caller.
    pub fn publish(
        &mut self,
        mode: PublicationMode,
        incoming: SlotContents,
    ) -> Result<(), RepositoryError> {
        incoming.validate_closure()?;
        let next_serial = self
            .slots
            .last()
            .and_then(|slot| slot.serial.checked_add(1))
            .ok_or_else(|| RepositoryError::new("slot serial space is exhausted"))?;
        match mode {
            PublicationMode::AppendToActive { retain_until } => {
                let active = self
                    .slots
                    .iter_mut()
                    .rev()
                    .find(|slot| slot.retain_until.is_none())
                    .ok_or_else(|| RepositoryError::new("repository has no current slot"))?;
                let mut replacement = active.contents.clone();
                replacement.merge_newer(&incoming)?;
                active.retain_until = Some(retain_until);
                self.slots.push(PublicationSlot {
                    serial: next_serial,
                    retain_until: None,
                    contents: replacement,
                });
            }
            PublicationMode::Rotate { retain_until } => {
                let oldest = self
                    .slots
                    .iter_mut()
                    .find(|slot| slot.retain_until.is_none())
                    .ok_or_else(|| RepositoryError::new("repository has no current slot"))?;
                oldest.retain_until = Some(retain_until);
                self.slots.push(PublicationSlot {
                    serial: next_serial,
                    retain_until: None,
                    contents: incoming,
                });
            }
        }
        self.metadata()?;
        Ok(())
    }

    /// Omits retired entries whose grace deadline has passed.
    pub fn prune_expired(&mut self, now: u64) -> Result<(), RepositoryError> {
        self.slots
            .retain(|slot| slot.retain_until.is_none_or(|deadline| deadline > now));
        self.metadata()?;
        Ok(())
    }

    /// Encodes and hashes every immutable table and constructs the next master.
    pub fn metadata(&self) -> Result<PublicationMetadata, RepositoryError> {
        let mut builds = BTreeMap::new();
        let mut reuses = BTreeMap::new();
        let mut object_lists = BTreeMap::new();
        let mut file_lists = BTreeMap::new();
        let mut descriptors = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            let build = BuildIndex::encode(&slot.contents.build_records())?;
            let reuse = ReuseIndex::encode(&slot.contents.reuse_records())?;
            let object_list = ObjectList::encode(slot.contents.objects());
            let file_list = FsFileList::encode(slot.contents.files());
            let build_hash = BuildIndexHash::digest(&build);
            let reuse_hash = ReuseIndexHash::digest(&reuse);
            let object_list_hash = ObjectListHash::digest(&object_list);
            let file_list_hash = FsFileListHash::digest(&file_list);
            builds.entry(build_hash).or_insert(build);
            reuses.entry(reuse_hash).or_insert(reuse);
            object_lists.entry(object_list_hash).or_insert(object_list);
            file_lists.entry(file_list_hash).or_insert(file_list);
            descriptors.push(Slot {
                serial: slot.serial,
                build: build_hash,
                reuse: reuse_hash,
                object_list: object_list_hash,
                file_list: file_list_hash,
                retain_until: slot.retain_until,
            });
        }
        Ok(PublicationMetadata {
            master: Master::new(self.data_base_url.clone(), descriptors)?,
            builds,
            reuses,
            object_lists,
            file_lists,
        })
    }

    /// Returns the current and retained logical slot states.
    pub fn slots(&self) -> &[PublicationSlot] {
        &self.slots
    }
}

fn collect_candidates<K: Ord>(
    records: impl IntoIterator<Item = (K, ObjectHash)>,
) -> BTreeMap<K, Vec<ObjectHash>> {
    let mut result = BTreeMap::<K, Vec<ObjectHash>>::new();
    for (key, object) in records {
        let candidates = result.entry(key).or_default();
        if !candidates.contains(&object) {
            candidates.push(object);
        }
    }
    result
}

fn flatten_candidates<K: Copy + Ord>(
    mappings: &BTreeMap<K, Vec<ObjectHash>>,
) -> Vec<(K, ObjectHash)> {
    mappings
        .iter()
        .flat_map(|(key, values)| values.iter().map(|value| (*key, *value)))
        .collect()
}

fn merge_candidates<K: Copy + Ord>(
    existing: &mut BTreeMap<K, Vec<ObjectHash>>,
    newer: &BTreeMap<K, Vec<ObjectHash>>,
) {
    for (key, candidates) in newer {
        let old = existing.remove(key).unwrap_or_default();
        let mut seen = HashSet::new();
        let merged = candidates
            .iter()
            .chain(old.iter())
            .copied()
            .filter(|candidate| seen.insert(*candidate))
            .collect();
        existing.insert(*key, merged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(byte: u8) -> ObjectHash {
        ObjectHash::from_bytes([byte; 32])
    }

    fn contents(key: u8, candidate: u8) -> SlotContents {
        SlotContents::new(
            [(BuildKey::from_bytes([key; 32]), object(candidate))],
            [],
            [object(candidate)],
            [],
        )
        .unwrap()
    }

    #[test]
    fn append_retires_old_active_and_prefers_new_candidate() {
        let mut state = PublicationState::initialize(
            Url::parse("https://example/data/").unwrap(),
            3,
            contents(1, 1),
        )
        .unwrap();
        state
            .publish(
                PublicationMode::AppendToActive { retain_until: 50 },
                contents(1, 2),
            )
            .unwrap();
        assert_eq!(state.slots().len(), 4);
        assert_eq!(state.slots()[2].retain_until, Some(50));
        assert_eq!(
            state.slots()[3].contents.build_records(),
            vec![
                (BuildKey::from_bytes([1; 32]), object(2)),
                (BuildKey::from_bytes([1; 32]), object(1)),
            ]
        );
    }

    #[test]
    fn rotate_preserves_current_slot_count_and_starts_fresh() {
        let mut state = PublicationState::initialize(
            Url::parse("https://example/data/").unwrap(),
            3,
            contents(1, 1),
        )
        .unwrap();
        state
            .publish(PublicationMode::Rotate { retain_until: 70 }, contents(2, 2))
            .unwrap();
        assert_eq!(
            state
                .slots()
                .iter()
                .filter(|slot| slot.retain_until.is_none())
                .count(),
            3
        );
        assert_eq!(state.slots()[0].retain_until, Some(70));
        assert_eq!(state.slots().last().unwrap().contents, contents(2, 2));
    }

    #[test]
    fn mappings_must_be_closed_by_object_list() {
        assert!(
            SlotContents::new([(BuildKey::from_bytes([1; 32]), object(1))], [], [], []).is_err()
        );
    }

    #[test]
    fn publication_metadata_roundtrips_to_state() {
        let state = PublicationState::initialize(
            Url::parse("https://example/data/").unwrap(),
            2,
            contents(1, 1),
        )
        .unwrap();
        let metadata = state.metadata().unwrap();
        assert_eq!(PublicationState::from_metadata(&metadata).unwrap(), state);
    }
}
