use std::path::{Path, PathBuf};

/// Workspace paths assigned to one concrete builder run.
///
/// A workspace belongs to a single per-run builder object. It contains the
/// subject log directory, the raw-log subdirectory, and a per-run temporary
/// directory. Allocation is handled outside `bobr-core`; this type is the
/// builder-facing value object passed to per-run builders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    log_dir: PathBuf,
    raw_log_dir: PathBuf,
    temp_dir: PathBuf,
}

impl Workspace {
    /// Creates a workspace from already allocated paths.
    pub fn new(log_dir: PathBuf, raw_log_dir: PathBuf, temp_dir: PathBuf) -> Self {
        Self {
            log_dir,
            raw_log_dir,
            temp_dir,
        }
    }

    /// Returns the per-run log directory.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Returns the per-run raw log directory.
    pub fn raw_log_dir(&self) -> &Path {
        &self.raw_log_dir
    }

    /// Returns the per-run temporary directory.
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }
}
