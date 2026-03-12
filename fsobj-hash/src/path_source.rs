use crate::error::Error;
use crate::hash::sha256_bytes;
use crate::node::{DirectoryEntry, DirectoryNode, FileNode, Node, SymlinkNode};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(crate) fn load_path(path: &Path) -> Result<Node, Error> {
    let metadata = fs::symlink_metadata(path).map_err(Error::Io)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(Error::UnsupportedRootSymlink {
            path: path.to_path_buf(),
        });
    }
    if file_type.is_file() {
        return read_file_node(path, metadata.permissions().mode());
    }
    if file_type.is_dir() {
        return read_directory_node(path);
    }
    Err(Error::UnsupportedFileType {
        path: path.to_path_buf(),
    })
}

fn read_file_node(path: &Path, mode: u32) -> Result<Node, Error> {
    let mut file = fs::File::open(path).map_err(Error::Io)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(Error::Io)?;
    Ok(Node::File(FileNode {
        executable: is_executable(mode),
        content_hash: sha256_bytes(&bytes),
        size: bytes.len() as u64,
    }))
}

fn read_symlink_node(path: &Path) -> Result<Node, Error> {
    let target = fs::read_link(path).map_err(Error::Io)?;
    Ok(Node::Symlink(SymlinkNode {
        target: os_str_bytes(target.as_os_str()).to_vec(),
    }))
}

fn read_directory_node(path: &Path) -> Result<Node, Error> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let child_path = entry.path();
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(&child_path).map_err(Error::Io)?;
        let file_type = metadata.file_type();
        let node = if file_type.is_file() {
            read_file_node(&child_path, metadata.permissions().mode())?
        } else if file_type.is_dir() {
            read_directory_node(&child_path)?
        } else if file_type.is_symlink() {
            read_symlink_node(&child_path)?
        } else {
            return Err(Error::UnsupportedFileType { path: child_path });
        };
        entries.push(DirectoryEntry {
            name: os_str_bytes(&name).to_vec(),
            node: Box::new(node),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Node::Directory(DirectoryNode { entries }))
}

fn is_executable(mode: u32) -> bool {
    (mode & 0o111) != 0
}

fn os_str_bytes(value: &OsStr) -> &[u8] {
    value.as_bytes()
}
