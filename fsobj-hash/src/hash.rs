use crate::error::EntryKind;
use crate::node::{DirectoryEntry, DirectoryNode, FileNode, Node, SymlinkNode};
use crate::object_hash::ObjectHash;
use sha2::{Digest, Sha256};

const FILE_TAG: &[u8] = b"fsobj:file:v1\0";
const DIR_TAG: &[u8] = b"fsobj:dir:v1\0";
const SYMLINK_TAG: &[u8] = b"fsobj:symlink:v1\0";

pub(crate) fn hash_node(node: &Node) -> ObjectHash {
    match node {
        Node::File(file) => hash_file(file),
        Node::Directory(directory) => hash_directory(directory),
        Node::Symlink(symlink) => hash_symlink(symlink),
    }
}

pub(crate) fn hash_file(file: &FileNode) -> ObjectHash {
    // A non-executable regular file hashes to exactly the SHA-256 of its bytes
    // (`content_hash` is that digest). This makes its object hash equal to the
    // digest upstreams publish (crates.io / `Cargo.lock`, `sha256sum`, release
    // digests), so a source can be pinned from a published checksum without
    // fetching it first. Executable files keep the tagged form below so that an
    // executable and a non-executable file with identical bytes stay distinct
    // objects; directory and symlink hashes keep their own tags, so a file hash
    // can never collide with them either.
    if !file.executable {
        return ObjectHash::from_bytes(file.content_hash);
    }
    let mut hasher = Sha256::new();
    hasher.update(FILE_TAG);
    hasher.update([u8::from(file.executable)]);
    hasher.update(file.size.to_be_bytes());
    hasher.update(file.content_hash);
    ObjectHash::from_bytes(hasher.finalize().into())
}

pub(crate) fn hash_symlink(symlink: &SymlinkNode) -> ObjectHash {
    let mut hasher = Sha256::new();
    hasher.update(SYMLINK_TAG);
    hasher.update((symlink.target.len() as u64).to_be_bytes());
    hasher.update(&symlink.target);
    ObjectHash::from_bytes(hasher.finalize().into())
}

pub(crate) fn hash_directory(directory: &DirectoryNode) -> ObjectHash {
    let mut hasher = Sha256::new();
    hasher.update(DIR_TAG);
    for entry in &directory.entries {
        hash_directory_entry(&mut hasher, entry);
    }
    ObjectHash::from_bytes(hasher.finalize().into())
}

pub(crate) fn hash_directory_entries(entries: &[DirectoryHashEntry<'_>]) -> ObjectHash {
    let mut hasher = Sha256::new();
    hasher.update(DIR_TAG);
    for entry in entries {
        let kind = match entry.kind {
            EntryKind::File => b'f',
            EntryKind::Directory => b'd',
            EntryKind::Symlink => b'l',
        };
        hasher.update([kind]);
        hasher.update((entry.name.len() as u64).to_be_bytes());
        hasher.update(entry.name);
        hasher.update(entry.hash.as_bytes());
    }
    ObjectHash::from_bytes(hasher.finalize().into())
}

pub(crate) struct DirectoryHashEntry<'a> {
    pub kind: EntryKind,
    pub name: &'a [u8],
    pub hash: ObjectHash,
}

fn hash_directory_entry(hasher: &mut Sha256, entry: &DirectoryEntry) {
    let kind = match entry.node.kind() {
        EntryKind::File => b'f',
        EntryKind::Directory => b'd',
        EntryKind::Symlink => b'l',
    };
    hasher.update([kind]);
    hasher.update((entry.name.len() as u64).to_be_bytes());
    hasher.update(&entry.name);
    hasher.update(hash_node(&entry.node).as_bytes());
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
