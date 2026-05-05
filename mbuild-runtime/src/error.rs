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

    #[error("subid configuration error: {0}")]
    SubidConfig(String),

    #[error("idmap range error: {0}")]
    OutOfRange(String),
}
