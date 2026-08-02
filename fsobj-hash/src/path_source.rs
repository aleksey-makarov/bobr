use crate::error::Error;
use crate::node::{DirectoryEntry, DirectoryNode, FileNode, Node, SymlinkNode};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const FILE_HASH_BUFFER_SIZE: usize = 64 * 1024;

fn io_at_path(path: &Path, action: &'static str, error: io::Error) -> Error {
    Error::IoAtPath {
        path: path.to_path_buf(),
        action,
        error,
    }
}

pub(crate) fn load_path(path: &Path) -> Result<Node, Error> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_at_path(path, "reading metadata", error))?;
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

pub(crate) fn load_directory_path(path: &Path) -> Result<Node, Error> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_at_path(path, "reading metadata", error))?;
    if !metadata.file_type().is_dir() {
        return Err(Error::UnsupportedFileType {
            path: path.to_path_buf(),
        });
    }
    read_directory_node(path)
}

fn read_file_node(path: &Path, mode: u32) -> Result<Node, Error> {
    let mut file = fs::File::open(path).map_err(|error| io_at_path(path, "opening file", error))?;
    let (content_hash, size) =
        sha256_reader(&mut file).map_err(|error| io_at_path(path, "reading file", error))?;
    Ok(Node::File(FileNode {
        executable: is_executable(mode),
        content_hash,
        size,
    }))
}

fn sha256_reader(reader: &mut (impl Read + ?Sized)) -> io::Result<([u8; 32], u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; FILE_HASH_BUFFER_SIZE];
    let mut size = 0_u64;

    loop {
        let bytes_read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => bytes_read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..bytes_read]);
        size += bytes_read as u64;
    }

    Ok((hasher.finalize().into(), size))
}

fn read_symlink_node(path: &Path) -> Result<Node, Error> {
    let target = fs::read_link(path).map_err(|error| io_at_path(path, "reading symlink", error))?;
    Ok(Node::Symlink(SymlinkNode {
        target: os_str_bytes(target.as_os_str()).to_vec(),
    }))
}

fn read_directory_node(path: &Path) -> Result<Node, Error> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|error| io_at_path(path, "reading directory", error))? {
        let entry = entry.map_err(|error| io_at_path(path, "reading directory entry", error))?;
        let child_path = entry.path();
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(&child_path)
            .map_err(|error| io_at_path(&child_path, "reading metadata", error))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha256_bytes;
    use std::io::Cursor;
    use tempfile::tempdir;

    struct BoundedReader<R> {
        inner: R,
        largest_buffer: usize,
    }

    impl<R: Read> Read for BoundedReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.largest_buffer = self.largest_buffer.max(buffer.len());
            self.inner.read(buffer)
        }
    }

    #[test]
    fn sha256_reader_hashes_empty_small_and_multiblock_inputs() {
        let payloads = [
            Vec::new(),
            b"small payload\n".to_vec(),
            vec![0x5a; FILE_HASH_BUFFER_SIZE * 3 + 17],
        ];

        for payload in payloads {
            let mut reader = Cursor::new(&payload);
            let (actual_hash, actual_size) = sha256_reader(&mut reader).unwrap();
            assert_eq!(actual_hash, sha256_bytes(&payload));
            assert_eq!(actual_size, payload.len() as u64);
        }
    }

    #[test]
    fn sha256_reader_uses_a_bounded_buffer() {
        let payload = vec![0xa5; FILE_HASH_BUFFER_SIZE * 2 + 1];
        let mut reader = BoundedReader {
            inner: Cursor::new(&payload),
            largest_buffer: 0,
        };

        let (actual_hash, actual_size) = sha256_reader(&mut reader).unwrap();

        assert_eq!(actual_hash, sha256_bytes(&payload));
        assert_eq!(actual_size, payload.len() as u64);
        assert_eq!(reader.largest_buffer, FILE_HASH_BUFFER_SIZE);
    }

    #[test]
    fn missing_file_error_includes_exact_path() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("nested").join("missing.txt");

        let error = match read_file_node(&nested, 0o644) {
            Ok(_) => panic!("expected read_file_node to fail for missing path"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert!(message.contains(&nested.display().to_string()), "{message}");
        assert!(message.contains("opening file"), "{message}");
    }
}
