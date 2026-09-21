use bobr_repo::{CurrentPublication, PublicationMetadata, RepositoryError, Slot, StoredKey};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RepositoryState {
    Ready,
    Empty,
    Missing,
}

#[derive(Debug, Serialize)]
pub(crate) struct StatusReport {
    state: RepositoryState,
    current_slots: usize,
    active_slot: Option<ActiveSlotStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    master_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    master_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ContentStatus>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    slots: Vec<LogicalSlotStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage: Option<StorageStatus>,
}

#[derive(Debug, Serialize)]
struct ActiveSlotStatus {
    serial: u64,
    content_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ContentStatus {
    unique_objects: usize,
    unique_files: usize,
    shared_objects: usize,
    shared_files: usize,
}

#[derive(Debug, Serialize)]
struct LogicalSlotStatus {
    serial: u64,
    state: SlotState,
    retain_until: Option<u64>,
    build_index: String,
    reuse_index: String,
    object_list: String,
    file_list: String,
    objects: usize,
    files: usize,
    exclusive_objects: usize,
    exclusive_files: usize,
    retention_expired: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum SlotState {
    Active,
    Sealed,
    Retained,
}

#[derive(Debug, Serialize)]
struct StorageStatus {
    keys: usize,
    bytes: u64,
    namespaces: BTreeMap<String, NamespaceStatus>,
    unreferenced: Vec<String>,
    unreferenced_bytes: u64,
    reclaimable_after_expired_prune_bytes: u64,
    slots: Vec<StorageSlotStatus>,
}

#[derive(Debug, Default, Serialize)]
struct NamespaceStatus {
    keys: usize,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct StorageSlotStatus {
    serial: u64,
    content_bytes: u64,
    referenced_bytes: u64,
    exclusive_bytes: u64,
    reclaimable_bytes_if_pruned_alone: u64,
}

impl StatusReport {
    pub(crate) fn empty(state: RepositoryState) -> Self {
        debug_assert!(matches!(
            state,
            RepositoryState::Empty | RepositoryState::Missing
        ));
        Self {
            state,
            current_slots: 0,
            active_slot: None,
            master_url: None,
            data_base_url: None,
            master_hash: None,
            key_id: None,
            content: None,
            slots: Vec::new(),
            storage: None,
        }
    }

    pub(crate) fn ready(
        master_url: &Url,
        publication: &CurrentPublication,
        stored: Option<&[StoredKey]>,
        now: u64,
    ) -> Result<Self, RepositoryError> {
        let master = &publication.metadata.master;
        let active_serial = master.active_slot().serial;
        let object_sets = master
            .slots()
            .iter()
            .map(|slot| metadata_hashes(&publication.metadata.object_lists, slot.object_list))
            .collect::<Vec<_>>();
        let file_sets = master
            .slots()
            .iter()
            .map(|slot| metadata_hashes(&publication.metadata.file_lists, slot.file_list))
            .collect::<Vec<_>>();
        let object_frequency = frequencies(&object_sets);
        let file_frequency = frequencies(&file_sets);
        let slots = master
            .slots()
            .iter()
            .enumerate()
            .map(|(index, slot)| LogicalSlotStatus {
                serial: slot.serial,
                state: if slot.retain_until.is_some() {
                    SlotState::Retained
                } else if slot.serial == active_serial {
                    SlotState::Active
                } else {
                    SlotState::Sealed
                },
                retain_until: slot.retain_until,
                build_index: slot.build.to_string(),
                reuse_index: slot.reuse.to_string(),
                object_list: slot.object_list.to_string(),
                file_list: slot.file_list.to_string(),
                objects: object_sets[index].len(),
                files: file_sets[index].len(),
                exclusive_objects: object_sets[index]
                    .iter()
                    .filter(|hash| object_frequency.get(*hash) == Some(&1))
                    .count(),
                exclusive_files: file_sets[index]
                    .iter()
                    .filter(|hash| file_frequency.get(*hash) == Some(&1))
                    .count(),
                retention_expired: slot.retain_until.is_some_and(|deadline| deadline <= now),
            })
            .collect();
        let storage = stored
            .map(|stored| storage_status(&publication.metadata, stored, now))
            .transpose()?;
        let active_content_bytes = storage.as_ref().and_then(|storage| {
            storage
                .slots
                .iter()
                .find(|slot| slot.serial == active_serial)
                .map(|slot| slot.content_bytes)
        });
        Ok(Self {
            state: RepositoryState::Ready,
            current_slots: master
                .slots()
                .iter()
                .filter(|slot| slot.retain_until.is_none())
                .count(),
            active_slot: Some(ActiveSlotStatus {
                serial: active_serial,
                content_bytes: active_content_bytes,
            }),
            master_url: Some(master_url.as_str().to_owned()),
            data_base_url: Some(master.data_base_url().as_str().to_owned()),
            master_hash: Some(publication.master_hash.to_string()),
            key_id: Some(hex(&publication.key_id)),
            content: Some(ContentStatus {
                unique_objects: object_frequency.len(),
                unique_files: file_frequency.len(),
                shared_objects: object_frequency
                    .values()
                    .filter(|count| **count > 1)
                    .count(),
                shared_files: file_frequency.values().filter(|count| **count > 1).count(),
            }),
            slots,
            storage,
        })
    }
}

fn storage_status(
    metadata: &PublicationMetadata,
    stored: &[StoredKey],
    now: u64,
) -> Result<StorageStatus, RepositoryError> {
    let live = live_keys(metadata);
    let stored_set = stored
        .iter()
        .map(|entry| entry.key.as_str())
        .collect::<BTreeSet<_>>();
    let missing = live
        .iter()
        .filter(|key| !stored_set.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        let preview = missing
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if missing.len() > 8 { ", ..." } else { "" };
        return Err(RepositoryError::new(format!(
            "repository is missing {} advertised key(s): {preview}{suffix}",
            missing.len()
        )));
    }

    let mut namespaces = BTreeMap::<String, NamespaceStatus>::new();
    for key in stored {
        let namespace = key.key.split('/').next().unwrap_or("other").to_owned();
        let entry = namespaces.entry(namespace).or_default();
        entry.bytes += key.size;
        entry.keys += 1;
    }
    let unreferenced = stored
        .iter()
        .filter(|entry| recognized_immutable(&entry.key) && !live.contains(&entry.key))
        .map(|entry| entry.key.clone())
        .collect::<Vec<_>>();
    let unreferenced_bytes = stored
        .iter()
        .filter(|entry| recognized_immutable(&entry.key) && !live.contains(&entry.key))
        .map(|entry| entry.size)
        .sum::<u64>();
    let sizes = stored
        .iter()
        .map(|entry| (entry.key.as_str(), entry.size))
        .collect::<HashMap<_, _>>();
    let slot_keys = metadata
        .master
        .slots()
        .iter()
        .map(|slot| live_slot_keys(metadata, slot))
        .collect::<Vec<_>>();
    let content_keys = metadata
        .master
        .slots()
        .iter()
        .map(|slot| slot_content_keys(metadata, slot))
        .collect::<Vec<_>>();
    let mut references = HashMap::<&str, usize>::new();
    for keys in &slot_keys {
        for key in keys {
            *references.entry(key.as_str()).or_default() += 1;
        }
    }
    let retained_expired = metadata
        .master
        .slots()
        .iter()
        .zip(&slot_keys)
        .filter(|(slot, _)| slot.retain_until.is_some_and(|deadline| deadline <= now))
        .flat_map(|(_, keys)| keys.iter().cloned())
        .collect::<BTreeSet<_>>();
    let not_expired = metadata
        .master
        .slots()
        .iter()
        .zip(&slot_keys)
        .filter(|(slot, _)| slot.retain_until.is_none_or(|deadline| deadline > now))
        .flat_map(|(_, keys)| keys.iter().cloned())
        .collect::<BTreeSet<_>>();
    let reclaimable_after_expired_prune_bytes = retained_expired
        .difference(&not_expired)
        .filter_map(|key| sizes.get(key.as_str()))
        .sum::<u64>();
    let slots = metadata
        .master
        .slots()
        .iter()
        .zip(&slot_keys)
        .zip(&content_keys)
        .map(|((slot, keys), content)| {
            let referenced_bytes = keys
                .iter()
                .filter_map(|key| sizes.get(key.as_str()))
                .sum::<u64>();
            let content_bytes = content
                .iter()
                .filter_map(|key| sizes.get(key.as_str()))
                .sum::<u64>();
            let exclusive_bytes = keys
                .iter()
                .filter(|key| references.get(key.as_str()) == Some(&1))
                .filter_map(|key| sizes.get(key.as_str()))
                .sum::<u64>();
            StorageSlotStatus {
                serial: slot.serial,
                content_bytes,
                referenced_bytes,
                exclusive_bytes,
                reclaimable_bytes_if_pruned_alone: if slot
                    .retain_until
                    .is_some_and(|deadline| deadline <= now)
                {
                    exclusive_bytes
                } else {
                    0
                },
            }
        })
        .collect();
    Ok(StorageStatus {
        keys: stored.len(),
        bytes: stored.iter().map(|entry| entry.size).sum(),
        namespaces,
        unreferenced,
        unreferenced_bytes,
        reclaimable_after_expired_prune_bytes,
        slots,
    })
}

fn live_keys(metadata: &PublicationMetadata) -> BTreeSet<String> {
    metadata
        .master
        .slots()
        .iter()
        .flat_map(|slot| live_slot_keys(metadata, slot))
        .collect()
}

fn live_slot_keys(metadata: &PublicationMetadata, slot: &Slot) -> BTreeSet<String> {
    let mut live = BTreeSet::from([
        format!("b/{}", slot.build),
        format!("r/{}", slot.reuse),
        format!("lo/{}", slot.object_list),
        format!("lf/{}", slot.file_list),
    ]);
    live.extend(slot_content_keys(metadata, slot));
    live
}

fn slot_content_keys(metadata: &PublicationMetadata, slot: &Slot) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    if let Some(bytes) = metadata.object_lists.get(&slot.object_list) {
        live.extend(
            bytes
                .chunks_exact(32)
                .map(|hash| format!("o/{}", hex(hash))),
        );
    }
    if let Some(bytes) = metadata.file_lists.get(&slot.file_list) {
        live.extend(
            bytes
                .chunks_exact(32)
                .map(|hash| format!("f/{}", hex(hash))),
        );
    }
    live
}

fn metadata_hashes<T: Ord + Copy>(values: &BTreeMap<T, Vec<u8>>, key: T) -> BTreeSet<[u8; 32]> {
    values
        .get(&key)
        .into_iter()
        .flat_map(|bytes| bytes.chunks_exact(32))
        .map(|hash| hash.try_into().expect("validated metadata hash record"))
        .collect()
}

fn frequencies(sets: &[BTreeSet<[u8; 32]>]) -> HashMap<[u8; 32], usize> {
    let mut frequencies = HashMap::new();
    for set in sets {
        for hash in set {
            *frequencies.entry(*hash).or_default() += 1;
        }
    }
    frequencies
}

fn recognized_immutable(key: &str) -> bool {
    ["b/", "r/", "lo/", "lf/", "o/", "f/"]
        .iter()
        .any(|namespace| key.starts_with(namespace))
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::ObjectHash;
    use bobr_repo::{
        BuildIndexHash, FsFileList, FsFileListHash, Master, MasterHash, ObjectList, ObjectListHash,
        ReuseIndexHash,
    };
    use bobr_store::fs_tree::FsFileHash;

    #[test]
    fn ready_status_counts_current_slots_and_active_content() {
        let shared = ObjectHash::from_bytes([21; 32]);
        let retained_only = ObjectHash::from_bytes([22; 32]);
        let active_only = ObjectHash::from_bytes([23; 32]);
        let active_file = FsFileHash::from_bytes([24; 32]);
        let retained = slot(1, Some(1), 1);
        let sealed = slot(2, None, 2);
        let active = slot(3, None, 3);
        let publication = publication(
            vec![retained.clone(), sealed.clone(), active.clone()],
            [
                (&retained, vec![shared, retained_only]),
                (&sealed, vec![shared]),
                (&active, vec![shared, active_only, active_only]),
            ],
            [
                (&retained, Vec::new()),
                (&sealed, Vec::new()),
                (&active, vec![active_file]),
            ],
        );
        let stored = complete_stored(
            &publication.metadata,
            &[
                (format!("o/{shared}"), 10),
                (format!("o/{retained_only}"), 20),
                (format!("o/{active_only}"), 30),
                (format!("f/{active_file}"), 40),
            ],
        );
        let report = StatusReport::ready(
            &Url::parse("https://example.test/master").unwrap(),
            &publication,
            Some(&stored),
            2,
        )
        .unwrap();
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["state"], "ready");
        assert_eq!(value["current_slots"], 2);
        assert_eq!(value["active_slot"]["serial"], 3);
        assert_eq!(value["active_slot"]["content_bytes"], 80);
        assert_eq!(value["storage"]["slots"][2]["content_bytes"], 80);
        assert_eq!(value["content"]["shared_objects"], 1);
        assert_eq!(value["storage"]["slots"][0]["exclusive_bytes"], 24);
        assert_eq!(
            value["storage"]["reclaimable_after_expired_prune_bytes"],
            24
        );
    }

    #[test]
    fn logical_status_marks_content_bytes_unknown() {
        let active = slot(1, None, 1);
        let publication = publication(
            vec![active.clone()],
            [(&active, Vec::new())],
            [(&active, Vec::new())],
        );
        let report = StatusReport::ready(
            &Url::parse("https://example.test/master").unwrap(),
            &publication,
            None,
            0,
        )
        .unwrap();
        let value = serde_json::to_value(report).unwrap();
        assert!(value["active_slot"]["content_bytes"].is_null());
        assert!(value.get("storage").is_none());
    }

    #[test]
    fn empty_and_missing_have_stable_top_level_shape() {
        for (state, name) in [
            (RepositoryState::Empty, "empty"),
            (RepositoryState::Missing, "missing"),
        ] {
            let value = serde_json::to_value(StatusReport::empty(state)).unwrap();
            assert_eq!(value["state"], name);
            assert_eq!(value["current_slots"], 0);
            assert!(value["active_slot"].is_null());
        }
    }

    #[test]
    fn missing_advertised_content_is_an_error() {
        let object = ObjectHash::from_bytes([21; 32]);
        let active = slot(1, None, 1);
        let publication = publication(
            vec![active.clone()],
            [(&active, vec![object])],
            [(&active, Vec::new())],
        );
        let mut stored = complete_stored(&publication.metadata, &[]);
        stored.retain(|entry| entry.key != format!("o/{object}"));
        let error = StatusReport::ready(
            &Url::parse("https://example.test/master").unwrap(),
            &publication,
            Some(&stored),
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains(&format!("o/{object}")));
    }

    fn publication<const O: usize, const F: usize>(
        slots: Vec<Slot>,
        objects: [(&Slot, Vec<ObjectHash>); O],
        files: [(&Slot, Vec<FsFileHash>); F],
    ) -> CurrentPublication {
        let master = Master::new(
            None,
            Url::parse("https://example.test/data/").unwrap(),
            slots,
        )
        .unwrap();
        CurrentPublication {
            master_hash: MasterHash::from_bytes([99; 32]),
            key_id: vec![7; 32],
            metadata: PublicationMetadata {
                master,
                builds: BTreeMap::new(),
                reuses: BTreeMap::new(),
                object_lists: objects
                    .into_iter()
                    .map(|(slot, hashes)| (slot.object_list, ObjectList::encode(hashes)))
                    .collect(),
                file_lists: files
                    .into_iter()
                    .map(|(slot, hashes)| (slot.file_list, FsFileList::encode(hashes)))
                    .collect(),
            },
        }
    }

    fn complete_stored(metadata: &PublicationMetadata, extra: &[(String, u64)]) -> Vec<StoredKey> {
        let mut sizes = live_keys(metadata)
            .into_iter()
            .map(|key| (key, 1))
            .collect::<BTreeMap<_, _>>();
        sizes.extend(extra.iter().cloned());
        sizes
            .into_iter()
            .map(|(key, size)| StoredKey { key, size })
            .collect()
    }

    fn slot(serial: u64, retain_until: Option<u64>, byte: u8) -> Slot {
        Slot {
            serial,
            build: BuildIndexHash::from_bytes([byte; 32]),
            reuse: ReuseIndexHash::from_bytes([byte.wrapping_add(10); 32]),
            object_list: ObjectListHash::from_bytes([byte.wrapping_add(20); 32]),
            file_list: FsFileListHash::from_bytes([byte.wrapping_add(30); 32]),
            retain_until,
        }
    }
}
