#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Idmap(#[from] IdmapError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("invalid runtime input: {0}")]
    InvalidInput(String),

    #[error("libcontainer error: {0}")]
    Libcontainer(String),

    #[error("executor error: {0}")]
    Executor(String),
}

#[derive(Debug, thiserror::Error)]
pub enum IdmapError {
    #[error("current user error: {0}")]
    CurrentUser(String),

    #[error("failed to read {kind} file '{path}': {source}")]
    SubidFileRead {
        kind: &'static str,
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[error("malformed {kind} line {line} in {source_label}: {message}")]
    MalformedSubidLine {
        kind: &'static str,
        source_label: String,
        line: usize,
        message: String,
    },

    #[error("{kind} line {line} in {source_label} has zero count")]
    ZeroSubidCount {
        kind: &'static str,
        source_label: String,
        line: usize,
    },

    #[error("{kind} line {line} in {source_label} overflows u32 range: base {base}, count {count}")]
    SubidRangeOverflow {
        kind: &'static str,
        source_label: String,
        line: usize,
        base: u32,
        count: u32,
    },

    #[error("{kind} not configured for user {username}; configure {path} and restart mbuild")]
    MissingSubidRange {
        kind: &'static str,
        username: String,
        path: String,
    },

    #[error("logical {kind} {logical} exceeds {subid_kind} range size {count}")]
    OutOfRange {
        kind: &'static str,
        subid_kind: &'static str,
        logical: u32,
        count: u32,
    },
}
