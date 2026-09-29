use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Stable policy-relevant category of a repository failure.
///
/// Callers should use this category to decide whether another repository may
/// be tried, whether an operation may be retried, and how cancellation is
/// reported. The human-readable message remains intentionally more specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepositoryErrorKind {
    /// Invalid caller configuration or arguments.
    Configuration,
    /// Network or remote-service failure.
    Transport {
        /// Whether retrying the same operation may succeed without changing
        /// its inputs.
        retryable: bool,
    },
    /// A signed value could not be authenticated by the pinned keys.
    Authentication,
    /// Authenticated repository data is malformed, inconsistent, or missing.
    InvalidRepository,
    /// Failure while reading or mutating local state.
    LocalIo,
    /// Failure while executing a local namespace runtime operation.
    Runtime,
    /// The caller cancelled the operation.
    Cancelled,
}

/// Failure while reading, validating, caching, or publishing a repository.
#[derive(Debug)]
pub struct RepositoryError {
    kind: RepositoryErrorKind,
    message: String,
    retry_after: Option<Duration>,
}

impl RepositoryError {
    /// Creates an invalid-repository error with a stable explanation.
    ///
    /// Format decoders use this compact constructor because malformed wire
    /// data is their overwhelmingly common failure mode. API boundaries should
    /// use the category-specific constructors below.
    pub fn new(message: impl Into<String>) -> Self {
        Self::invalid_repository(message)
    }

    /// Creates an error in an explicit policy category.
    pub fn with_kind(kind: RepositoryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Creates a caller-configuration error.
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::with_kind(RepositoryErrorKind::Configuration, message)
    }

    /// Creates a transport error and records whether it is retryable.
    pub fn transport(message: impl Into<String>, retryable: bool) -> Self {
        Self::with_kind(RepositoryErrorKind::Transport { retryable }, message)
    }

    /// Creates a retryable transport error with an optional server-requested
    /// delay before the next attempt.
    pub fn retryable_transport(message: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            kind: RepositoryErrorKind::Transport { retryable: true },
            message: message.into(),
            retry_after,
        }
    }

    /// Creates an authentication error.
    pub fn authentication(message: impl Into<String>) -> Self {
        Self::with_kind(RepositoryErrorKind::Authentication, message)
    }

    /// Creates an invalid-repository error.
    pub fn invalid_repository(message: impl Into<String>) -> Self {
        Self::with_kind(RepositoryErrorKind::InvalidRepository, message)
    }

    /// Creates a local-I/O error.
    pub fn local_io(message: impl Into<String>) -> Self {
        Self::with_kind(RepositoryErrorKind::LocalIo, message)
    }

    /// Creates a local runtime error.
    pub fn runtime(message: impl Into<String>) -> Self {
        Self::with_kind(RepositoryErrorKind::Runtime, message)
    }

    /// Creates a cancellation error.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::with_kind(RepositoryErrorKind::Cancelled, message)
    }

    /// Returns the stable policy category.
    pub fn kind(&self) -> RepositoryErrorKind {
        self.kind
    }

    /// Returns whether an immediate retry may succeed without changed inputs.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            RepositoryErrorKind::Transport { retryable: true }
        )
    }

    /// Returns the server-requested retry delay, when one was supplied.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
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
        Self::local_io(format!("I/O error: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{RepositoryError, RepositoryErrorKind};
    use std::time::Duration;

    #[test]
    fn categories_and_retry_policy_are_typed() {
        let transient = RepositoryError::transport("timeout", true);
        assert_eq!(
            transient.kind(),
            RepositoryErrorKind::Transport { retryable: true }
        );
        assert!(transient.is_retryable());
        assert_eq!(transient.retry_after(), None);

        let rate_limited =
            RepositoryError::retryable_transport("slow down", Some(Duration::from_secs(3)));
        assert_eq!(rate_limited.retry_after(), Some(Duration::from_secs(3)));

        let invalid = RepositoryError::new("bad metadata");
        assert_eq!(invalid.kind(), RepositoryErrorKind::InvalidRepository);
        assert!(!invalid.is_retryable());
    }
}
