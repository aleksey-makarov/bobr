use crate::{CasError, fsutil};
use fsobj_hash::{EntryKind, LeafIndex, ObjectHash};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectLeafIndex {
    entries: BTreeMap<String, ObjectLeafIndexEntry>,
}

impl ObjectLeafIndex {
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn from_fsobj_index(index: &LeafIndex) -> Option<Self> {
        let mut entries = BTreeMap::new();
        for entry in index.entries() {
            let path = String::from_utf8(entry.path.clone()).ok()?;
            let kind = ObjectLeafKind::from_entry_kind(entry.kind)?;
            entries.insert(
                path,
                ObjectLeafIndexEntry {
                    kind,
                    node_hash: entry.node_hash,
                },
            );
        }
        Some(Self { entries })
    }

    pub fn insert(&mut self, path: impl Into<String>, kind: ObjectLeafKind, node_hash: ObjectHash) {
        self.entries
            .insert(path.into(), ObjectLeafIndexEntry { kind, node_hash });
    }

    pub fn get(&self, path: &str) -> Option<&ObjectLeafIndexEntry> {
        self.entries.get(path)
    }

    pub fn entries(&self) -> &BTreeMap<String, ObjectLeafIndexEntry> {
        &self.entries
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectLeafIndexEntry {
    pub kind: ObjectLeafKind,
    pub node_hash: ObjectHash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectLeafKind {
    File,
    Symlink,
}

impl ObjectLeafKind {
    pub fn from_entry_kind(kind: EntryKind) -> Option<Self> {
        match kind {
            EntryKind::File => Some(Self::File),
            EntryKind::Symlink => Some(Self::Symlink),
            EntryKind::Directory => None,
        }
    }

    pub fn to_entry_kind(self) -> EntryKind {
        match self {
            Self::File => EntryKind::File,
            Self::Symlink => EntryKind::Symlink,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonLine {
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
    hash: String,
}

pub fn read_object_leaf_index(path: &Path) -> Result<Option<ObjectLeafIndex>, CasError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CasError::Io(format!(
                "failed to read object index '{}': {error}",
                path.display()
            )));
        }
    };

    let mut index = ObjectLeafIndex::empty();
    for (line_idx, line) in content.lines().enumerate() {
        if line.is_empty() {
            return Err(CasError::Serialization(format!(
                "object index '{}' has empty line {}",
                path.display(),
                line_idx + 1
            )));
        }
        let parsed = serde_json::from_str::<JsonLine>(line).map_err(|error| {
            CasError::Serialization(format!(
                "failed to parse object index '{}' line {}: {error}",
                path.display(),
                line_idx + 1
            ))
        })?;
        let kind = match parsed.entry_type.as_str() {
            "file" => ObjectLeafKind::File,
            "symlink" => ObjectLeafKind::Symlink,
            other => {
                return Err(CasError::Serialization(format!(
                    "object index '{}' line {} has unknown type '{}'",
                    path.display(),
                    line_idx + 1,
                    other
                )));
            }
        };
        let node_hash = ObjectHash::from_str(&parsed.hash).map_err(|error| {
            CasError::Serialization(format!(
                "object index '{}' line {} has invalid hash '{}': {error}",
                path.display(),
                line_idx + 1,
                parsed.hash
            ))
        })?;
        index.insert(parsed.path, kind, node_hash);
    }

    Ok(Some(index))
}

pub fn write_object_leaf_index(path: &Path, index: &ObjectLeafIndex) -> Result<(), CasError> {
    let mut content = String::new();
    for (entry_path, entry) in index.entries() {
        let entry_type = match entry.kind {
            ObjectLeafKind::File => "file",
            ObjectLeafKind::Symlink => "symlink",
        };
        let line = JsonLine {
            path: entry_path.clone(),
            entry_type: entry_type.to_string(),
            hash: entry.node_hash.to_string(),
        };
        content.push_str(&serde_json::to_string(&line).map_err(|error| {
            CasError::Serialization(format!(
                "failed to serialize object index entry '{}': {error}",
                entry_path
            ))
        })?);
        content.push('\n');
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CasError::Io(format!(
                "failed to create object index directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    fsutil::write_atomic(path, &content)
        .map_err(|error| CasError::Io(format!("failed to write object index: {error}")))
}
