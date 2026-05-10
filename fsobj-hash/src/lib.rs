mod error;
mod hash;
mod node;
mod normalize;
mod object_hash;
mod path_source;
mod tar_source;

pub use error::{EntryKind, Error, InvalidPathReason, TarEntryKind};
pub use object_hash::ObjectHash;

use crate::node::{DirectoryEntry, DirectoryNode, FileNode, Node};
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub fn hash_path(path: impl AsRef<Path>) -> Result<ObjectHash, Error> {
    let node = path_source::load_path(path.as_ref())?;
    Ok(hash::hash_node(&node))
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
    let root = path_source::load_directory_path(root_dir.as_ref())?;
    let manifest = Node::File(FileNode {
        executable: false,
        content_hash: hash::sha256_bytes(manifest_bytes),
        size: manifest_bytes.len() as u64,
    });
    let object = Node::Directory(DirectoryNode {
        entries: vec![
            DirectoryEntry {
                name: b"manifest.jsonl".to_vec(),
                node: Box::new(manifest),
            },
            DirectoryEntry {
                name: b"root".to_vec(),
                node: Box::new(root),
            },
        ],
    });
    Ok(hash::hash_node(&object))
}
