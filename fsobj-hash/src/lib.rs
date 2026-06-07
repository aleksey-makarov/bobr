mod error;
mod hash;
mod hex_hash;
mod node;
mod normalize;
mod object_hash;
mod path_source;
mod tar_source;

pub use error::{EntryKind, Error, InvalidPathReason, TarEntryKind};
pub use hex_hash::ParseHexHashError;
pub use object_hash::ObjectHash;

use crate::hash::DirectoryHashEntry;
use crate::node::{DirectoryEntry, DirectoryNode, FileNode, Node, SymlinkNode};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedPathHash {
    pub object_hash: ObjectHash,
    pub leaf_index: LeafIndex,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeafIndex {
    entries: Vec<LeafIndexEntry>,
}

impl LeafIndex {
    pub fn entries(&self) -> &[LeafIndexEntry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<LeafIndexEntry> {
        self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafIndexEntry {
    pub path: Vec<u8>,
    pub kind: EntryKind,
    pub node_hash: ObjectHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntryHash<'a> {
    pub name: &'a [u8],
    pub kind: EntryKind,
    pub node_hash: ObjectHash,
}

pub fn hash_path(path: impl AsRef<Path>) -> Result<ObjectHash, Error> {
    let node = path_source::load_path(path.as_ref())?;
    Ok(hash::hash_node(&node))
}

pub fn hash_path_with_leaf_index(path: impl AsRef<Path>) -> Result<IndexedPathHash, Error> {
    let node = path_source::load_path(path.as_ref())?;
    let object_hash = hash::hash_node(&node);
    let mut entries = Vec::new();
    collect_leaf_index(&node, &mut Vec::new(), &mut entries);
    Ok(IndexedPathHash {
        object_hash,
        leaf_index: LeafIndex { entries },
    })
}

pub fn hash_tar_reader<R: Read>(reader: R) -> Result<ObjectHash, Error> {
    let node = tar_source::load_tar_reader(reader)?;
    Ok(hash::hash_node(&node))
}

pub fn hash_tar_file(path: impl AsRef<Path>) -> Result<ObjectHash, Error> {
    let file = File::open(path.as_ref()).map_err(Error::Io)?;
    hash_tar_reader(file)
}

pub fn hash_fs_tree_object(
    manifest_bytes: &[u8],
    root_dir: impl AsRef<Path>,
) -> Result<ObjectHash, Error> {
    hash_fs_tree_object_with_extra_files(manifest_bytes, root_dir, &[])
}

pub fn hash_fs_tree_object_with_extra_files(
    manifest_bytes: &[u8],
    root_dir: impl AsRef<Path>,
    extra_files: &[(&[u8], &[u8])],
) -> Result<ObjectHash, Error> {
    let root = path_source::load_directory_path(root_dir.as_ref())?;
    let manifest = Node::File(FileNode {
        executable: false,
        content_hash: hash::sha256_bytes(manifest_bytes),
        size: manifest_bytes.len() as u64,
    });
    let mut entries = vec![
        DirectoryEntry {
            name: b"manifest.jsonl".to_vec(),
            node: Box::new(manifest),
        },
        DirectoryEntry {
            name: b"root".to_vec(),
            node: Box::new(root),
        },
    ];
    for (name, content) in extra_files {
        entries.push(DirectoryEntry {
            name: name.to_vec(),
            node: Box::new(Node::File(FileNode {
                executable: false,
                content_hash: hash::sha256_bytes(content),
                size: content.len() as u64,
            })),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let object = Node::Directory(DirectoryNode { entries });
    Ok(hash::hash_node(&object))
}

pub fn hash_file_node(executable: bool, size: u64, content_hash: [u8; 32]) -> ObjectHash {
    hash::hash_file(&FileNode {
        executable,
        content_hash,
        size,
    })
}

pub fn hash_file_bytes(executable: bool, bytes: &[u8]) -> ObjectHash {
    hash_file_node(executable, bytes.len() as u64, hash::sha256_bytes(bytes))
}

pub fn hash_symlink_node(target: &[u8]) -> ObjectHash {
    hash::hash_symlink(&SymlinkNode {
        target: target.to_vec(),
    })
}

pub fn hash_directory_node(entries: &[DirectoryEntryHash<'_>]) -> ObjectHash {
    let mut sorted = entries
        .iter()
        .map(|entry| DirectoryHashEntry {
            kind: entry.kind,
            name: entry.name,
            hash: entry.node_hash,
        })
        .collect::<Vec<_>>();
    sorted.sort_by(|left, right| left.name.cmp(right.name));
    hash::hash_directory_entries(&sorted)
}

pub fn hash_fs_tree_object_from_hashes(
    manifest_file_hash: ObjectHash,
    root_hash: ObjectHash,
) -> ObjectHash {
    hash_directory_node(&[
        DirectoryEntryHash {
            name: b"manifest.jsonl",
            kind: EntryKind::File,
            node_hash: manifest_file_hash,
        },
        DirectoryEntryHash {
            name: b"root",
            kind: EntryKind::Directory,
            node_hash: root_hash,
        },
    ])
}

fn collect_leaf_index(node: &Node, path: &mut Vec<u8>, entries: &mut Vec<LeafIndexEntry>) {
    match node {
        Node::File(_) | Node::Symlink(_) => {
            entries.push(LeafIndexEntry {
                path: path.clone(),
                kind: node.kind(),
                node_hash: hash::hash_node(node),
            });
        }
        Node::Directory(directory) => {
            for entry in &directory.entries {
                let original_len = path.len();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&entry.name);
                collect_leaf_index(&entry.node, path, entries);
                path.truncate(original_len);
            }
        }
    }
}
