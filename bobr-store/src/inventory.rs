//! Validated immutable inventory of a completed store.

use crate::fs_tree::{FsFileHash, FsTreeEntry, hash_fs_file_path, read_manifest_if_marked};
use crate::refs::parse_object_target;
use crate::store::{BUILDS_DIR, FS_FILES_DIR, OBJECTS_DIR, REUSES_DIR};
use crate::{ReadOnlyStore, StoreError};
use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_runtime::runtime_provider::RuntimeProvider;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// One validated ordinary object in a store inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredObject {
    /// Content identity named by the store path.
    pub hash: ObjectHash,
    /// Canonical absolute source path.
    pub path: PathBuf,
}

/// One validated filesystem-file object in a store inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFsFile {
    /// Filesystem-file identity named by the store path.
    pub hash: FsFileHash,
    /// Canonical absolute source path.
    pub path: PathBuf,
}

/// Complete validated content and key mappings of one quiescent store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreInventory {
    /// Build-key mappings in key order.
    pub builds: Vec<(BuildKey, ObjectHash)>,
    /// Reuse-key mappings in key order.
    pub reuses: Vec<(ReuseKey, ObjectHash)>,
    /// Ordinary objects in hash order.
    pub objects: Vec<StoredObject>,
    /// Filesystem files in hash order.
    pub files: Vec<StoredFsFile>,
}

impl ReadOnlyStore {
    /// Scans and validates all publishable content and mappings in the store.
    ///
    /// The caller must keep the store quiescent for the duration of the scan.
    /// Derived trees, logs, records, and convenience references are ignored.
    pub fn inventory(&self) -> Result<StoreInventory, StoreError> {
        let objects = scan_objects(self.root())?;
        let files = scan_fs_files(self.root())?;
        let object_hashes = objects.iter().map(|object| object.hash).collect();
        let file_hashes = files.iter().map(|file| file.hash).collect();
        validate_fs_tree_closure(&objects, &file_hashes)?;
        let builds = scan_mappings::<BuildKey>(self.root(), BUILDS_DIR, &object_hashes)?;
        let reuses = scan_mappings::<ReuseKey>(self.root(), REUSES_DIR, &object_hashes)?;
        Ok(StoreInventory {
            builds,
            reuses,
            objects,
            files,
        })
    }

    /// Scans the store through `runtime`, preserving logical fs-file ownership
    /// when the host user must enter a namespace to observe it.
    pub fn inventory_with_runtime(
        &self,
        runtime: &RuntimeProvider,
    ) -> Result<StoreInventory, StoreError> {
        let output = tempfile::NamedTempFile::new().map_err(|error| {
            StoreError::Io(format!("failed to create inventory result file: {error}"))
        })?;
        runtime
            .run(
                &StoreInventoryFunction,
                StoreInventoryInput {
                    root: self.root().to_path_buf(),
                    output: output.path().to_path_buf(),
                },
            )
            .map_err(|error| StoreError::Io(format!("store inventory runtime failed: {error}")))?;
        let file = fs::File::open(output.path())
            .map_err(|error| StoreError::Io(format!("failed to open inventory result: {error}")))?;
        serde_json::from_reader(file).map_err(|error| {
            StoreError::InvalidData(format!("failed to decode inventory result: {error}"))
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoreInventoryInput {
    root: PathBuf,
    output: PathBuf,
}

pub(crate) struct StoreInventoryFunction;

impl RuntimeFunction for StoreInventoryFunction {
    type Input = StoreInventoryInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "bobr_store_inventory"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        let store = ReadOnlyStore::open(&input.root)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let inventory = store
            .inventory()
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let file = fs::File::create(&input.output)?;
        serde_json::to_writer(file, &inventory)
            .map_err(|error| RuntimeError::new(error.to_string()))
    }
}

fn scan_objects(root: &Path) -> Result<Vec<StoredObject>, StoreError> {
    let directory = root.join(OBJECTS_DIR);
    let mut objects = Vec::new();
    for entry in read_sorted(&directory)? {
        let name = utf8_name(&entry.path())?;
        let expected = name.parse::<ObjectHash>().map_err(|error| {
            invalid(format!(
                "invalid object entry '{}' in '{}': {error}",
                name,
                directory.display()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            io(format!(
                "failed to inspect '{}': {error}",
                entry.path().display()
            ))
        })?;
        if !metadata.file_type().is_file() && !metadata.file_type().is_dir() {
            return Err(invalid(format!(
                "object '{}' is neither a regular file nor a directory",
                entry.path().display()
            )));
        }
        let actual = fsobj_hash::hash_path(entry.path()).map_err(|error| {
            StoreError::Hashing(format!(
                "failed to hash object '{}': {error}",
                entry.path().display()
            ))
        })?;
        if actual != expected {
            return Err(invalid(format!(
                "object '{}' has hash '{actual}', expected '{expected}'",
                entry.path().display()
            )));
        }
        objects.push(StoredObject {
            hash: expected,
            path: entry.path(),
        });
    }
    Ok(objects)
}

fn scan_fs_files(root: &Path) -> Result<Vec<StoredFsFile>, StoreError> {
    let directory = root.join(FS_FILES_DIR);
    let mut files = Vec::new();
    for shard in read_sorted(&directory)? {
        let shard_name = utf8_name(&shard.path())?;
        if shard_name.len() != 2 || !is_lower_hex(&shard_name) {
            return Err(invalid(format!(
                "invalid fs-files shard '{}'",
                shard.path().display()
            )));
        }
        let metadata = fs::symlink_metadata(shard.path()).map_err(|error| {
            io(format!(
                "failed to inspect '{}': {error}",
                shard.path().display()
            ))
        })?;
        if !metadata.file_type().is_dir() {
            return Err(invalid(format!(
                "fs-files shard '{}' is not a directory",
                shard.path().display()
            )));
        }
        for entry in read_sorted(&shard.path())? {
            let name = utf8_name(&entry.path())?;
            let expected = name.parse::<FsFileHash>().map_err(|error| {
                invalid(format!(
                    "invalid fs-file entry '{}': {error}",
                    entry.path().display()
                ))
            })?;
            if !name.starts_with(&shard_name) {
                return Err(invalid(format!(
                    "fs-file '{}' is in the wrong shard",
                    entry.path().display()
                )));
            }
            let actual = hash_fs_file_path(&entry.path())?;
            if actual != expected {
                return Err(invalid(format!(
                    "fs-file '{}' has hash '{actual}', expected '{expected}'",
                    entry.path().display()
                )));
            }
            files.push(StoredFsFile {
                hash: expected,
                path: entry.path(),
            });
        }
    }
    files.sort_by_key(|file| file.hash);
    Ok(files)
}

fn validate_fs_tree_closure(
    objects: &[StoredObject],
    files: &BTreeSet<FsFileHash>,
) -> Result<(), StoreError> {
    for object in objects {
        let Some(manifest) = read_manifest_if_marked(&object.path)? else {
            continue;
        };
        for hash in manifest.entries().iter().filter_map(|entry| match entry {
            FsTreeEntry::File { hash, .. } => Some(*hash),
            _ => None,
        }) {
            if !files.contains(&hash) {
                return Err(invalid(format!(
                    "fs-tree object '{}' references missing fs-file '{hash}'",
                    object.hash
                )));
            }
        }
    }
    Ok(())
}

trait MappingKey: Copy + Ord + std::str::FromStr {
    fn parse_target(kind: &str, path: &Path, target: &Path) -> Result<ObjectHash, StoreError> {
        parse_object_target(kind, path, target)
    }
}

impl MappingKey for BuildKey {}
impl MappingKey for ReuseKey {}

fn scan_mappings<K: MappingKey>(
    root: &Path,
    name: &str,
    objects: &BTreeSet<ObjectHash>,
) -> Result<Vec<(K, ObjectHash)>, StoreError>
where
    <K as std::str::FromStr>::Err: std::fmt::Display,
{
    let directory = root.join(name);
    let mut mappings = Vec::new();
    for entry in read_sorted(&directory)? {
        let key_name = utf8_name(&entry.path())?;
        let key = key_name.parse::<K>().map_err(|error| {
            invalid(format!(
                "invalid {name} key '{}' in '{}': {error}",
                key_name,
                directory.display()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            io(format!(
                "failed to inspect '{}': {error}",
                entry.path().display()
            ))
        })?;
        if !metadata.file_type().is_symlink() {
            return Err(invalid(format!(
                "{name} entry '{}' is not a symlink",
                entry.path().display()
            )));
        }
        let target = fs::read_link(entry.path()).map_err(|error| {
            io(format!(
                "failed to read '{}': {error}",
                entry.path().display()
            ))
        })?;
        let object = K::parse_target(name, &entry.path(), &target)?;
        if !objects.contains(&object) {
            return Err(invalid(format!(
                "{name} entry '{}' references absent object '{object}'",
                entry.path().display()
            )));
        }
        mappings.push((key, object));
    }
    mappings.sort_by_key(|(key, _)| *key);
    Ok(mappings)
}

fn read_sorted(directory: &Path) -> Result<Vec<fs::DirEntry>, StoreError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| io(format!("failed to read '{}': {error}", directory.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io(format!("failed to read '{}': {error}", directory.display())))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn utf8_name(path: &Path) -> Result<String, StoreError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("store entry '{}' is not UTF-8", path.display())))
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid(message: String) -> StoreError {
    StoreError::InvalidData(message)
}

fn io(message: String) -> StoreError {
    StoreError::Io(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, import_build};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn inventories_valid_file_object_and_mapping() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let store = Store::create(&root).unwrap();
        let source = root.join("source");
        fs::write(&source, b"hello").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        let build = BuildKey::from_bytes([7; 32]);
        let published = import_build(
            &store,
            build,
            ReuseKey::from_bytes([8; 32]),
            Vec::new(),
            &source,
            "test",
            "run",
        )
        .unwrap();

        let inventory = ReadOnlyStore::open(&root).unwrap().inventory().unwrap();
        assert_eq!(inventory.builds, vec![(build, published)]);
        assert_eq!(inventory.objects.len(), 1);
        assert_eq!(inventory.objects[0].hash, published);
        assert_eq!(inventory.reuses.len(), 1);
        assert!(inventory.files.is_empty());
        assert_eq!(
            inventory,
            ReadOnlyStore::open(&root)
                .unwrap()
                .inventory_with_runtime(&RuntimeProvider::host())
                .unwrap()
        );
    }

    #[test]
    fn rejects_mapping_to_absent_object() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        Store::create(&root).unwrap();
        let key = BuildKey::from_bytes([3; 32]);
        std::os::unix::fs::symlink(
            format!("../objects/{}", ObjectHash::from_bytes([4; 32])),
            root.join(BUILDS_DIR).join(key.to_string()),
        )
        .unwrap();
        let error = ReadOnlyStore::open(&root).unwrap().inventory().unwrap_err();
        assert!(error.to_string().contains("references absent object"));
    }
}
