#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use mbuild_core::{FsTreeEntry, FsTreeManifest};
use mbuild_runtime::{MbuildIdmap, RuntimeError, apply_ownership_batch};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use tempfile::tempdir;
use tracing_subscriber::EnvFilter;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn apply_ownership_batch_materializes_logical_owners_and_modes() -> TestResult<()> {
    let _guard = runtime_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    init_tracing();
    let idmap = MbuildIdmap::from_host_environment()?;
    let temp = tempdir()?;
    let workspace = temp.path().join("workspace");
    let target = temp.path().join("target");
    fs::create_dir(&workspace)?;
    fs::create_dir(&target)?;
    fs::create_dir(target.join("dir"))?;
    fs::write(target.join("file"), b"file")?;
    symlink("file", target.join("link"))?;

    let manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("dir", 1, 1, 0o700),
        FsTreeEntry::file("file", 1, 1, 0o640),
        FsTreeEntry::symlink("link", 1, 1, "target"),
    ])?;

    apply_ownership_batch(&target, &manifest, &idmap, &workspace)?;

    assert_owner_and_mode(&target, idmap.current_uid(), idmap.current_gid(), 0o755)?;
    assert_owner_and_mode(
        target.join("dir"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o700,
    )?;
    assert_owner_and_mode(
        target.join("file"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o640,
    )?;
    let link = fs::symlink_metadata(target.join("link"))?;
    assert!(link.file_type().is_symlink());
    assert_eq!(link.uid(), idmap.physical_uid(1)?);
    assert_eq!(link.gid(), idmap.physical_gid(1)?);
    assert_runtime_workspace_empty(&workspace)?;

    Ok(())
}

#[test]
fn apply_ownership_batch_defers_parent_directory_until_descendants() -> TestResult<()> {
    let _guard = runtime_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    init_tracing();
    let idmap = MbuildIdmap::from_host_environment()?;
    let temp = tempdir()?;
    let workspace = temp.path().join("workspace");
    let target = temp.path().join("target");
    fs::create_dir(&workspace)?;
    fs::create_dir(&target)?;
    fs::create_dir(target.join("locked"))?;
    fs::create_dir(target.join("locked/nested"))?;
    fs::write(target.join("locked/nested/file"), b"file")?;
    fs::set_permissions(target.join("locked"), fs::Permissions::from_mode(0o700))?;

    let manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("locked", 1, 1, 0o711),
        FsTreeEntry::directory("locked/nested", 1, 1, 0o711),
        FsTreeEntry::file("locked/nested/file", 1, 1, 0o600),
    ])?;

    apply_ownership_batch(&target, &manifest, &idmap, &workspace)?;

    assert_owner_and_mode(
        target.join("locked"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o711,
    )?;
    assert_owner_and_mode(
        target.join("locked/nested"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o711,
    )?;
    assert_owner_and_mode(
        target.join("locked/nested/file"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o600,
    )?;
    assert_runtime_workspace_empty(&workspace)?;

    Ok(())
}

#[test]
fn apply_ownership_batch_returns_structured_executor_error() -> TestResult<()> {
    let _guard = runtime_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    init_tracing();
    let idmap = MbuildIdmap::from_host_environment()?;
    let temp = tempdir()?;
    let workspace = temp.path().join("workspace");
    let target = temp.path().join("target");
    fs::create_dir(&workspace)?;
    fs::create_dir(&target)?;
    fs::create_dir(target.join("entry"))?;

    let manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::file("entry", 0, 0, 0o644),
    ])?;

    let error = apply_ownership_batch(&target, &manifest, &idmap, &workspace)
        .expect_err("kind mismatch should surface structured executor error");

    assert!(
        matches!(error, RuntimeError::Executor(_)),
        "expected RuntimeError::Executor, got {error:?}: {error}"
    );
    assert!(error.to_string().contains("kind error at /target/entry"));
    assert!(error.to_string().contains("expected file"));
    assert_runtime_workspace_empty(&workspace)?;

    Ok(())
}

fn runtime_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn assert_owner_and_mode(path: impl AsRef<Path>, uid: u32, gid: u32, mode: u32) -> TestResult<()> {
    let metadata = fs::symlink_metadata(path.as_ref())?;
    assert_eq!(metadata.uid(), uid);
    assert_eq!(metadata.gid(), gid);
    assert_eq!(metadata.permissions().mode() & 0o7777, mode);
    Ok(())
}

fn assert_runtime_workspace_empty(workspace: &Path) -> TestResult<()> {
    assert_empty_dir(&workspace.join("state"))?;
    assert_empty_dir(&workspace.join("bundles"))?;
    Ok(())
}

fn assert_empty_dir(path: &Path) -> TestResult<()> {
    assert!(path.is_dir());
    assert!(fs::read_dir(path)?.next().is_none());
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
