use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    UnsupportedRootSymlink {
        path: PathBuf,
    },
    UnsupportedFileType {
        path: PathBuf,
    },
    TarRead(std::io::Error),
    InvalidArchivePath {
        path: PathBuf,
        reason: InvalidPathReason,
    },
    DuplicateEntry {
        path: PathBuf,
    },
    KindConflict {
        path: PathBuf,
        existing: EntryKind,
        new: EntryKind,
    },
    UnsupportedTarEntry {
        path: PathBuf,
        kind: TarEntryKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidPathReason {
    AbsolutePath,
    ParentTraversal,
    EmptyPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TarEntryKind {
    HardLink,
    BlockDevice,
    CharDevice,
    Fifo,
    Socket,
    Other,
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File => f.write_str("file"),
            Self::Directory => f.write_str("directory"),
            Self::Symlink => f.write_str("symlink"),
        }
    }
}

impl fmt::Display for InvalidPathReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AbsolutePath => f.write_str("absolute path is not allowed"),
            Self::ParentTraversal => f.write_str("parent traversal is not allowed"),
            Self::EmptyPath => f.write_str("empty path is not allowed"),
        }
    }
}

impl fmt::Display for TarEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HardLink => f.write_str("hard link"),
            Self::BlockDevice => f.write_str("block device"),
            Self::CharDevice => f.write_str("char device"),
            Self::Fifo => f.write_str("fifo"),
            Self::Socket => f.write_str("socket"),
            Self::Other => f.write_str("unsupported tar entry"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::UnsupportedRootSymlink { path } => {
                write!(f, "root symlink is not supported: {}", path.display())
            }
            Self::UnsupportedFileType { path } => {
                write!(f, "unsupported file type: {}", path.display())
            }
            Self::TarRead(error) => write!(f, "tar read error: {error}"),
            Self::InvalidArchivePath { path, reason } => {
                write!(f, "invalid archive path '{}': {reason}", path.display())
            }
            Self::DuplicateEntry { path } => {
                write!(f, "duplicate archive entry: {}", path.display())
            }
            Self::KindConflict {
                path,
                existing,
                new,
            } => {
                write!(
                    f,
                    "kind conflict for '{}': existing {existing}, new {new}",
                    path.display()
                )
            }
            Self::UnsupportedTarEntry { path, kind } => {
                write!(f, "unsupported tar entry at '{}': {kind}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::TarRead(error) => Some(error),
            _ => None,
        }
    }
}
