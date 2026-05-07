use crate::error::RuntimeError;
use libcontainer::oci_spec::runtime::Spec;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;
use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct Bundle {
    dir: PathBuf,
    rootfs_dir: PathBuf,
    error_log_path: PathBuf,
    result_log_path: PathBuf,
}

impl Bundle {
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn rootfs_dir(&self) -> &Path {
        &self.rootfs_dir
    }

    pub(crate) fn error_log_path(&self) -> &Path {
        &self.error_log_path
    }

    pub(crate) fn result_log_path(&self) -> &Path {
        &self.result_log_path
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.dir) {
            warn!(
                "failed to remove runtime bundle directory '{}': {error}",
                self.dir.display()
            );
        }
    }
}

pub(crate) fn create_bundle(workspace: &Path, spec: &Spec) -> Result<Bundle, RuntimeError> {
    if !workspace.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "runtime bundle workspace '{}' must exist and be a directory",
            workspace.display()
        )));
    }

    let bundles_dir = workspace.join("bundles");
    fs::create_dir_all(&bundles_dir)?;

    let dir = bundles_dir.join(Uuid::new_v4().simple().to_string());
    fs::create_dir(&dir)?;

    let rootfs_dir = dir.join("rootfs");
    let error_log_path = rootfs_dir.join("error.json");
    let result_log_path = rootfs_dir.join("result.json");
    let bundle = Bundle {
        dir,
        rootfs_dir,
        error_log_path,
        result_log_path,
    };

    fs::create_dir(bundle.rootfs_dir())?;
    fs::create_dir(bundle.rootfs_dir().join("dev"))?;
    fs::create_dir(bundle.rootfs_dir().join("proc"))?;
    fs::create_dir(bundle.rootfs_dir().join("target"))?;
    fs::File::create(bundle.error_log_path())?;
    fs::File::create(bundle.result_log_path())?;

    let config_path = bundle.dir().join("config.json");
    spec.save(&config_path).map_err(|error| {
        RuntimeError::Libcontainer(format!(
            "failed to write OCI spec config '{}': {error}",
            config_path.display()
        ))
    })?;

    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{idmap::MbuildIdmap, spec::build_ownership_spec};
    use tempfile::tempdir;

    #[test]
    fn create_bundle_writes_layout_and_config() {
        let workspace = tempdir().unwrap();
        let spec = test_spec();
        let bundle = create_bundle(workspace.path(), &spec).unwrap();

        assert!(bundle.dir().is_dir());
        assert_eq!(
            bundle.dir().parent(),
            Some(workspace.path().join("bundles").as_path())
        );
        assert_eq!(bundle.rootfs_dir(), &bundle.dir().join("rootfs"));
        assert_eq!(
            bundle.error_log_path(),
            &bundle.dir().join("rootfs").join("error.json")
        );
        assert_eq!(
            bundle.result_log_path(),
            &bundle.dir().join("rootfs").join("result.json")
        );
        assert!(bundle.rootfs_dir().join("dev").is_dir());
        assert!(bundle.rootfs_dir().join("proc").is_dir());
        assert!(bundle.rootfs_dir().join("target").is_dir());
        assert_eq!(fs::read(bundle.error_log_path()).unwrap(), b"");
        assert_eq!(fs::read(bundle.result_log_path()).unwrap(), b"");

        let loaded = Spec::load(bundle.dir().join("config.json")).unwrap();
        assert_eq!(loaded, spec);
    }

    #[test]
    fn drop_removes_only_bundle_dir() {
        let workspace = tempdir().unwrap();
        let bundles_dir = workspace.path().join("bundles");
        let bundle = create_bundle(workspace.path(), &test_spec()).unwrap();
        let bundle_dir = bundle.dir().to_path_buf();

        drop(bundle);

        assert!(!bundle_dir.exists());
        assert!(workspace.path().is_dir());
        assert!(bundles_dir.is_dir());
    }

    #[test]
    fn create_bundle_allocates_unique_dirs() {
        let workspace = tempdir().unwrap();
        let first = create_bundle(workspace.path(), &test_spec()).unwrap();
        let second = create_bundle(workspace.path(), &test_spec()).unwrap();

        assert_ne!(first.dir(), second.dir());
        assert!(first.dir().is_dir());
        assert!(second.dir().is_dir());
    }

    #[test]
    fn create_bundle_rejects_missing_workspace() {
        let workspace = tempdir().unwrap();
        let missing = workspace.path().join("missing");
        let error = create_bundle(&missing, &test_spec()).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(!missing.exists());
    }

    #[test]
    fn create_bundle_rejects_non_directory_workspace() {
        let workspace = tempdir().unwrap();
        let file = workspace.path().join("workspace-file");
        fs::write(&file, b"not a directory").unwrap();

        let error = create_bundle(&file, &test_spec()).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(
            !workspace
                .path()
                .join("workspace-file")
                .join("bundles")
                .exists()
        );
    }

    fn test_spec() -> Spec {
        let idmap = MbuildIdmap::for_tests(1000, 1001, 100000, 65536, 200000, 65536);
        build_ownership_spec(&idmap, Path::new("/tmp/mbuild-runtime-target")).unwrap()
    }
}
