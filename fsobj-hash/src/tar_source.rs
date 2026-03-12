use crate::error::{Error, TarEntryKind};
use crate::hash::sha256_bytes;
use crate::node::{FileNode, Node, SymlinkNode};
use crate::normalize::{PendingEntry, build_tree, insert_entry, normalize_archive_path};
use std::collections::BTreeMap;
use std::io::Read;
use tar::{Archive, EntryType};

pub(crate) fn load_tar_reader<R: Read>(reader: R) -> Result<Node, Error> {
    let mut archive = Archive::new(reader);
    let mut entries = BTreeMap::new();

    for entry_result in archive.entries().map_err(Error::TarRead)? {
        let mut entry = entry_result.map_err(Error::TarRead)?;
        let entry_type = entry.header().entry_type();
        let path = normalize_archive_path(&entry.path_bytes())?;
        let pending = match entry_type {
            t if t.is_file() => {
                let mode = entry.header().mode().map_err(Error::TarRead)?;
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).map_err(Error::TarRead)?;
                PendingEntry::File(FileNode {
                    executable: (mode & 0o111) != 0,
                    content_hash: sha256_bytes(&bytes),
                    size: bytes.len() as u64,
                })
            }
            t if t.is_dir() => PendingEntry::Directory,
            t if t.is_symlink() => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| Error::UnsupportedTarEntry {
                        path: path_buf(&path),
                        kind: TarEntryKind::Other,
                    })?;
                PendingEntry::Symlink(SymlinkNode {
                    target: target.into_owned(),
                })
            }
            t if t.is_hard_link() => {
                return Err(Error::UnsupportedTarEntry {
                    path: path_buf(&path),
                    kind: TarEntryKind::HardLink,
                });
            }
            t if t.is_block_special() => {
                return Err(Error::UnsupportedTarEntry {
                    path: path_buf(&path),
                    kind: TarEntryKind::BlockDevice,
                });
            }
            t if t.is_character_special() => {
                return Err(Error::UnsupportedTarEntry {
                    path: path_buf(&path),
                    kind: TarEntryKind::CharDevice,
                });
            }
            t if t.is_fifo() => {
                return Err(Error::UnsupportedTarEntry {
                    path: path_buf(&path),
                    kind: TarEntryKind::Fifo,
                });
            }
            _ => {
                return Err(Error::UnsupportedTarEntry {
                    path: path_buf(&path),
                    kind: classify_other(entry_type),
                });
            }
        };
        insert_entry(&mut entries, path, pending)?;
    }

    Ok(build_tree(entries))
}

fn classify_other(entry_type: EntryType) -> TarEntryKind {
    match entry_type.as_byte() {
        b's' => TarEntryKind::Socket,
        _ => TarEntryKind::Other,
    }
}

fn path_buf(path: &[Vec<u8>]) -> std::path::PathBuf {
    let joined = path
        .iter()
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>()
        .join("/");
    std::path::PathBuf::from(joined)
}
