use std::fmt;

/// Error returned by operations that work with a `bobr` store.
///
/// `StoreError` is intentionally store-oriented rather than a generic
/// filesystem error facade. Variants group failures by how callers should think
/// about them: invalid caller input, invalid data already present in the store,
/// unsupported filesystem state, IO failures, and object hashing failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The caller passed an invalid argument to a store operation.
    ///
    /// Examples include non-store temporary paths, non-absolute fs-tree roots,
    /// invalid ref names, or entries that cannot form a valid manifest.
    InvalidInput(String),
    /// Store data or a store-owned serialized format is malformed.
    ///
    /// This is used for corrupt object records, invalid reference targets,
    /// non-canonical fs-tree manifests, and similar data-integrity failures.
    InvalidData(String),
    /// The requested operation reached a filesystem case the store does not
    /// support.
    ///
    /// Examples include trying to scan unsupported file types into an fs-tree.
    Unsupported(String),
    /// An operating-system IO operation failed.
    ///
    /// The message includes the affected store path whenever the caller can be
    /// told a meaningful path.
    Io(String),
    /// Computing an object or filesystem content hash failed.
    Hashing(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message)
            | Self::InvalidData(message)
            | Self::Unsupported(message)
            | Self::Io(message)
            | Self::Hashing(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for StoreError {}

pub(crate) fn map_fsutil_error(error: crate::fsutil::FsUtilError) -> StoreError {
    StoreError::Io(error.to_string())
}
