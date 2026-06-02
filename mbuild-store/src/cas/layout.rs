use super::CasError;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const OBJECTS_DIR: &str = "objects";
pub(crate) const BUILDS_DIR: &str = "builds";
pub(crate) const RESULTS_DIR: &str = "results";
pub(crate) const REUSES_DIR: &str = "reuses";
pub(crate) const OBJECT_REFS_DIR: &str = "object-refs";
pub(crate) const RESULT_REFS_DIR: &str = "result-refs";
pub(crate) const FS_FILES_DIR: &str = "fs-files";
pub(crate) const FS_TREES_DIR: &str = "fs-trees";

#[derive(Debug, Clone)]
pub struct StoreLayout {
    pub root: PathBuf,
    pub objects: PathBuf,
    pub builds: PathBuf,
    pub reuses: PathBuf,
    pub results: PathBuf,
    pub object_refs: PathBuf,
    pub result_refs: PathBuf,
    pub fs_files: PathBuf,
    pub fs_trees: PathBuf,
}

impl StoreLayout {
    pub fn discover(root: &Path) -> Result<Self, CasError> {
        let layout = Self {
            root: root.to_path_buf(),
            objects: root.join(OBJECTS_DIR),
            builds: root.join(BUILDS_DIR),
            reuses: root.join(REUSES_DIR),
            results: root.join(RESULTS_DIR),
            object_refs: root.join(OBJECT_REFS_DIR),
            result_refs: root.join(RESULT_REFS_DIR),
            fs_files: root.join(FS_FILES_DIR),
            fs_trees: root.join(FS_TREES_DIR),
        };
        layout.ensure()?;
        Ok(layout)
    }

    pub fn discover_in_cwd() -> Result<Self, CasError> {
        let cwd = env::current_dir()
            .map_err(|error| CasError::Io(format!("failed to get current directory: {error}")))?;
        Self::discover(&cwd)
    }

    fn ensure(&self) -> Result<(), CasError> {
        ensure_dir(&self.root, "mbuild root")?;
        ensure_dir(&self.objects, "objects")?;
        ensure_dir(&self.builds, "builds")?;
        ensure_dir(&self.reuses, "reuses")?;
        ensure_dir(&self.results, "results")?;
        ensure_dir(&self.object_refs, "object-refs")?;
        ensure_dir(&self.result_refs, "result-refs")?;
        ensure_dir(&self.fs_files, "fs-files")?;
        ensure_dir(&self.fs_trees, "fs-trees")?;
        Ok(())
    }
}

fn ensure_dir(path: &Path, label: &str) -> Result<(), CasError> {
    fs::create_dir_all(path).map_err(|error| {
        CasError::Io(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}
