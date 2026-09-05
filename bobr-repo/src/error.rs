use std::fmt;

/// Failure while reading, validating, caching, or publishing a repository.
#[derive(Debug)]
pub struct RepositoryError {
    message: String,
}

impl RepositoryError {
    /// Creates an error with a stable human-readable explanation.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RepositoryError {}

impl From<std::io::Error> for RepositoryError {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!("I/O error: {error}"))
    }
}
