use bobr_core::RunError;
use bobr_store::StoreError;
use std::fmt;

/// Why a realization request failed. Each variant carries a human-readable
/// message; the variant classifies the failure.
#[derive(Debug)]
pub enum ExecutionError {
    /// Structurally valid but semantically invalid request, such as a zero job
    /// count.
    InvalidRequest(String),
    /// A node references a builder tag that is not registered.
    UnknownBuilder(String),
    /// The request could not be decoded or failed schema/shape validation.
    RequestLoad(String),
    /// The run was cancelled before completing.
    Cancelled(String),
    /// A subject failed to build or the dependency graph could not be realized.
    Build(String),
    /// A content-addressed store operation failed.
    Store(String),
    /// A run-scoped workspace or scratch operation failed.
    Run(String),
}

impl From<RunError> for ExecutionError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::InvalidRequest(message) => Self::InvalidRequest(message),
            RunError::Failed(message) => Self::Run(message),
        }
    }
}

impl ExecutionError {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid-request",
            Self::UnknownBuilder(_) => "unknown-builder",
            Self::RequestLoad(_) => "request-load",
            Self::Cancelled(_) => "cancelled",
            Self::Build(_) => "build",
            Self::Store(_) => "store",
            Self::Run(_) => "run",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::UnknownBuilder(message)
            | Self::RequestLoad(message)
            | Self::Cancelled(message)
            | Self::Build(message)
            | Self::Store(message)
            | Self::Run(message) => message,
        }
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for ExecutionError {}

impl From<StoreError> for ExecutionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error.to_string())
    }
}
