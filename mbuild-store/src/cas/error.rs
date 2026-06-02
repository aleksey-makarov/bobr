use crate::fsutil;
use std::fmt;

#[derive(Debug)]
pub enum CasError {
    Io(String),
    InvalidInput(String),
    Hashing(String),
    Serialization(String),
}

impl fmt::Display for CasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message)
            | Self::InvalidInput(message)
            | Self::Hashing(message)
            | Self::Serialization(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CasError {}

pub(crate) fn map_fsutil_error(error: fsutil::FsUtilError) -> CasError {
    CasError::Io(error.to_string())
}
