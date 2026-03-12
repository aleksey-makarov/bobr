use crate::error::EntryKind;

pub(crate) enum Node {
    File(FileNode),
    Directory(DirectoryNode),
    Symlink(SymlinkNode),
}

#[derive(Clone)]
pub(crate) struct FileNode {
    pub executable: bool,
    pub content_hash: [u8; 32],
    pub size: u64,
}

pub(crate) struct DirectoryNode {
    pub entries: Vec<DirectoryEntry>,
}

pub(crate) struct DirectoryEntry {
    pub name: Vec<u8>,
    pub node: Box<Node>,
}

#[derive(Clone)]
pub(crate) struct SymlinkNode {
    pub target: Vec<u8>,
}

impl Node {
    pub(crate) fn kind(&self) -> EntryKind {
        match self {
            Self::File(_) => EntryKind::File,
            Self::Directory(_) => EntryKind::Directory,
            Self::Symlink(_) => EntryKind::Symlink,
        }
    }
}
