use std::fmt;
use std::path::PathBuf;

/// Error from hashing a filesystem path or tar archive.
#[derive(Debug)]
pub enum Error {
    /// An I/O error with no specific path attached.
    Io(std::io::Error),
    /// An I/O error tied to a specific path and action.
    IoAtPath {
        /// Path the operation was on.
        path: PathBuf,
        /// What was being attempted (e.g. "read", "stat").
        action: &'static str,
        /// The underlying I/O error.
        error: std::io::Error,
    },
    /// The root path is itself a symlink, which is not supported.
    UnsupportedRootSymlink {
        /// The offending root path.
        path: PathBuf,
    },
    /// A path is an unsupported file type (not a file, directory, or symlink).
    UnsupportedFileType {
        /// The offending path.
        path: PathBuf,
    },
    /// An error reading the tar stream.
    TarRead(std::io::Error),
    /// A tar entry has a disallowed path (see [`InvalidPathReason`]).
    InvalidArchivePath {
        /// The offending archive path.
        path: PathBuf,
        /// Why the path was rejected.
        reason: InvalidPathReason,
    },
    /// A tar archive lists the same path more than once.
    DuplicateEntry {
        /// The duplicated path.
        path: PathBuf,
    },
    /// A path appears with two conflicting kinds (e.g. file vs directory).
    KindConflict {
        /// The conflicting path.
        path: PathBuf,
        /// Kind already recorded for the path.
        existing: EntryKind,
        /// Kind newly encountered for the path.
        new: EntryKind,
    },
    /// A tar entry uses a type that cannot be hashed (see [`TarEntryKind`]).
    UnsupportedTarEntry {
        /// The offending path.
        path: PathBuf,
        /// The unsupported tar entry type.
        kind: TarEntryKind,
    },
}

/// Kind of a filesystem object that can appear in a hashed tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
}

/// Why a tar archive entry path was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidPathReason {
    /// The path is absolute.
    AbsolutePath,
    /// The path contains a `..` parent-traversal component.
    ParentTraversal,
    /// The path is empty.
    EmptyPath,
}

/// A tar entry type that `fsobj-hash` does not hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TarEntryKind {
    /// Hard link.
    HardLink,
    /// Block device node.
    BlockDevice,
    /// Character device node.
    CharDevice,
    /// FIFO (named pipe).
    Fifo,
    /// Unix domain socket.
    Socket,
    /// Any other unsupported entry type.
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
            Self::IoAtPath {
                path,
                action,
                error,
            } => {
                write!(
                    f,
                    "io error at '{}', while {action}: {error}",
                    path.display()
                )
            }
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
            Self::IoAtPath { error, .. } => Some(error),
            _ => None,
        }
    }
}
