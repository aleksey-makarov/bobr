use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    InvalidInput(String),
    InvalidData(String),
    Unsupported(String),
    Io(String),
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
