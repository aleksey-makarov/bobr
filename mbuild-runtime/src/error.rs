/// Errors returned by runtime operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Host idmap discovery or logical-to-physical id translation failed.
    #[error(transparent)]
    Idmap(#[from] IdmapError),

    /// Filesystem I/O failed while preparing or running a runtime operation.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The caller supplied an invalid runtime input.
    #[error("invalid runtime input: {0}")]
    InvalidInput(String),

    /// The host environment is missing prerequisites needed before starting a
    /// runtime container.
    #[error("runtime preflight failed: {0}")]
    Preflight(String),

    /// `libcontainer` failed while creating, starting, waiting for, or deleting
    /// a runtime container.
    #[error("libcontainer error: {0}")]
    Libcontainer(String),

    /// Child-side runtime execution failed and returned a structured report or
    /// non-zero lifecycle status.
    #[error("executor error: {0}")]
    Executor(String),
}

/// Errors returned while discovering or applying the host idmap.
#[derive(Debug, thiserror::Error)]
pub enum IdmapError {
    /// The current effective user could not be resolved.
    #[error("current user error: {0}")]
    CurrentUser(String),

    /// A subuid or subgid file could not be read.
    #[error("failed to read {kind} file '{path}': {source}")]
    SubidFileRead {
        /// The subid file kind, either `subuid` or `subgid`.
        kind: &'static str,
        /// Path to the subid file.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A subuid or subgid file contains a malformed line.
    #[error("malformed {kind} line {line} in {source_label}: {message}")]
    MalformedSubidLine {
        /// The subid file kind, either `subuid` or `subgid`.
        kind: &'static str,
        /// Human-readable source label for the parsed file.
        source_label: String,
        /// One-based line number.
        line: usize,
        /// Detailed parse failure.
        message: String,
    },

    /// A configured subid range has zero length.
    #[error("{kind} line {line} in {source_label} has zero count")]
    ZeroSubidCount {
        /// The subid file kind, either `subuid` or `subgid`.
        kind: &'static str,
        /// Human-readable source label for the parsed file.
        source_label: String,
        /// One-based line number.
        line: usize,
    },

    /// A configured subid range overflows the `u32` id space.
    #[error("{kind} line {line} in {source_label} overflows u32 range: base {base}, count {count}")]
    SubidRangeOverflow {
        /// The subid file kind, either `subuid` or `subgid`.
        kind: &'static str,
        /// Human-readable source label for the parsed file.
        source_label: String,
        /// One-based line number.
        line: usize,
        /// First physical id in the configured range.
        base: u32,
        /// Number of ids in the configured range.
        count: u32,
    },

    /// No matching subid range exists for the current user.
    #[error("{kind} not configured for user {username}; configure {path} and restart mbuild")]
    MissingSubidRange {
        /// The subid file kind, either `subuid` or `subgid`.
        kind: &'static str,
        /// Current username used for the lookup.
        username: String,
        /// Path to the subid file that was searched.
        path: String,
    },

    /// A logical id is outside the configured subid range.
    #[error("logical {kind} {logical} exceeds {subid_kind} range size {count}")]
    OutOfRange {
        /// Logical id kind, either `uid` or `gid`.
        kind: &'static str,
        /// Backing subid range kind, either `subuid` or `subgid`.
        subid_kind: &'static str,
        /// Logical id requested by the caller.
        logical: u32,
        /// Size of the configured backing range.
        count: u32,
    },
}
