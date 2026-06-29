use std::fmt;

/// Error returned by a builder implementation.
#[derive(Debug)]
pub enum BuilderError {
    /// The recipe object is malformed for this builder.
    InvalidRecipe(String),
    /// The build was cancelled before completing.
    Cancelled(String),
    /// The builder ran but failed.
    ExecutionFailed(String),
    /// The requested builder operation is not implemented.
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
