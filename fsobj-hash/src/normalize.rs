use crate::error::{EntryKind, Error, InvalidPathReason};
use crate::node::{DirectoryEntry, DirectoryNode, FileNode, Node, SymlinkNode};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub(crate) type PathComponents = Vec<Vec<u8>>;

#[derive(Clone)]
pub(crate) enum PendingEntry {
    File(FileNode),
    Directory,
    Symlink(SymlinkNode),
}

impl PendingEntry {
    fn kind(&self) -> EntryKind {
        match self {
            Self::File(_) => EntryKind::File,
            Self::Directory => EntryKind::Directory,
            Self::Symlink(_) => EntryKind::Symlink,
        }
    }
}

pub(crate) fn normalize_archive_path(raw: &[u8]) -> Result<PathComponents, Error> {
    let original = String::from_utf8_lossy(raw).into_owned();
    let normalized_raw = if let Some(stripped) = raw.strip_prefix(b"./") {
        stripped
    } else {
        raw
    };
    if normalized_raw.is_empty() {
        return Err(Error::InvalidArchivePath {
            path: PathBuf::from(original),
            reason: InvalidPathReason::EmptyPath,
        });
    }
    if normalized_raw.starts_with(b"/") {
        return Err(Error::InvalidArchivePath {
            path: PathBuf::from(original),
            reason: InvalidPathReason::AbsolutePath,
        });
    }

    let mut components = Vec::new();
    for component in normalized_raw.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            return Err(Error::InvalidArchivePath {
                path: PathBuf::from(original),
                reason: InvalidPathReason::ParentTraversal,
            });
        }
        components.push(component.to_vec());
    }

    if components.is_empty() {
        return Err(Error::InvalidArchivePath {
            path: PathBuf::from(original),
            reason: InvalidPathReason::EmptyPath,
        });
    }

    Ok(components)
}

pub(crate) fn insert_entry(
    entries: &mut BTreeMap<PathComponents, PendingEntry>,
    components: PathComponents,
    pending: PendingEntry,
) -> Result<(), Error> {
    insert_implicit_parents(entries, &components)?;
    match entries.get(&components) {
        None => {
            entries.insert(components, pending);
            Ok(())
        }
        Some(PendingEntry::Directory) if matches!(pending, PendingEntry::Directory) => Ok(()),
        Some(existing) if existing.kind() == pending.kind() => Err(Error::DuplicateEntry {
            path: path_buf_from_components(&components),
        }),
        Some(existing) => Err(Error::KindConflict {
            path: path_buf_from_components(&components),
            existing: existing.kind(),
            new: pending.kind(),
        }),
    }
}

fn insert_implicit_parents(
    entries: &mut BTreeMap<PathComponents, PendingEntry>,
    components: &[Vec<u8>],
) -> Result<(), Error> {
    for depth in 1..components.len() {
        let parent = components[..depth].to_vec();
        match entries.get(&parent) {
            None => {
                entries.insert(parent, PendingEntry::Directory);
            }
            Some(PendingEntry::Directory) => {}
            Some(existing) => {
                return Err(Error::KindConflict {
                    path: path_buf_from_components(&parent),
                    existing: existing.kind(),
                    new: EntryKind::Directory,
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn build_tree(entries: BTreeMap<PathComponents, PendingEntry>) -> Node {
    let mut child_names: BTreeMap<PathComponents, BTreeSet<Vec<u8>>> = BTreeMap::new();
    for path in entries.keys() {
        let parent = path[..path.len().saturating_sub(1)].to_vec();
        if let Some(name) = path.last() {
            child_names.entry(parent).or_default().insert(name.clone());
        }
    }
    build_directory(entries, &child_names, &[])
}

fn build_directory(
    entries: BTreeMap<PathComponents, PendingEntry>,
    child_names: &BTreeMap<PathComponents, BTreeSet<Vec<u8>>>,
    prefix: &[Vec<u8>],
) -> Node {
    let mut out = Vec::new();
    if let Some(children) = child_names.get(prefix) {
        for name in children {
            let mut child_path = prefix.to_vec();
            child_path.push(name.clone());
            let pending = entries.get(&child_path).expect("child entry must exist");
            let node = match pending.clone() {
                PendingEntry::File(file) => Node::File(file),
                PendingEntry::Symlink(link) => Node::Symlink(link),
                PendingEntry::Directory => {
                    build_directory(entries.clone(), child_names, &child_path)
                }
            };
            out.push(DirectoryEntry {
                name: name.clone(),
                node: Box::new(node),
            });
        }
    }
    Node::Directory(DirectoryNode { entries: out })
}

fn path_buf_from_components(components: &[Vec<u8>]) -> PathBuf {
    let joined = components
        .iter()
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>()
        .join("/");
    PathBuf::from(joined)
}
