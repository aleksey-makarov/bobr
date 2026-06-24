use std::fmt;

/// Error returned by a builder implementation.
#[derive(Debug)]
pub enum BuilderError {
    InvalidRecipe(String),
    Cancelled(String),
    ExecutionFailed(String),
    NotImplemented(String),
}

impl fmt::Display for BuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipe(message)
            | Self::Cancelled(message)
            | Self::ExecutionFailed(message)
            | Self::NotImplemented(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BuilderError {}
