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

fn hash_file(file: &FileNode) -> ObjectHash {
    let mut hasher = Sha256::new();
    hasher.update(FILE_TAG);
    hasher.update([u8::from(file.executable)]);
    hasher.update(file.size.to_be_bytes());
    hasher.update(file.content_hash);
    ObjectHash(hasher.finalize().into())
}

fn hash_symlink(symlink: &SymlinkNode) -> ObjectHash {
    let mut hasher = Sha256::new();
    hasher.update(SYMLINK_TAG);
    hasher.update((symlink.target.len() as u64).to_be_bytes());
    hasher.update(&symlink.target);
    ObjectHash(hasher.finalize().into())
}

fn hash_directory(directory: &DirectoryNode) -> ObjectHash {
    let mut hasher = Sha256::new();
    hasher.update(DIR_TAG);
    for entry in &directory.entries {
        hash_directory_entry(&mut hasher, entry);
    }
    ObjectHash(hasher.finalize().into())
}

fn hash_directory_entry(hasher: &mut Sha256, entry: &DirectoryEntry) {
    let kind = match entry.node.kind() {
        crate::error::EntryKind::File => b'f',
        crate::error::EntryKind::Directory => b'd',
        crate::error::EntryKind::Symlink => b'l',
    };
    hasher.update([kind]);
    hasher.update((entry.name.len() as u64).to_be_bytes());
    hasher.update(&entry.name);
    hasher.update(hash_node(&entry.node).as_bytes());
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
