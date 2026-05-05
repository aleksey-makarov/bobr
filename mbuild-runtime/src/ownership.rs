//! Public ownership materialization entrypoints.

use crate::{ExecutorErrorReport, write_executor_error_report};
use libcontainer::oci_spec::runtime::Spec;
use libcontainer::workload::{
    Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError,
};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use nix::unistd::{Gid, Uid, chown};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct OwnershipExecutor {
    target_inside: PathBuf,
    entries: Vec<FsTreeEntry>,
    error_log_inside: PathBuf,
}

impl OwnershipExecutor {
    pub fn new(manifest: &FsTreeManifest) -> Self {
        Self::with_paths(
            manifest,
            PathBuf::from("/target"),
            PathBuf::from("/error.json"),
        )
    }

    pub(crate) fn with_paths(
        manifest: &FsTreeManifest,
        target_inside: PathBuf,
        error_log_inside: PathBuf,
    ) -> Self {
        Self {
            target_inside,
            entries: manifest.entries().to_vec(),
            error_log_inside,
        }
    }

    pub(crate) fn apply(&self) -> Result<(), ExecutorErrorReport> {
        let entries = self.validate_entries()?;

        for entry in &entries {
            match entry.kind {
                EntryKind::File | EntryKind::Directory => {
                    chown_if_needed(&entry.path, entry.uid, entry.gid)?;
                }
                EntryKind::Symlink => {}
            }
        }

        for entry in &entries {
            if entry.kind == EntryKind::Symlink {
                lchown_if_needed(&entry.path, entry.uid, entry.gid)?;
            }
        }

        for entry in &entries {
            if entry.kind == EntryKind::File {
                chmod(&entry.path, entry.mode.expect("file entry has mode"))?;
            }
        }

        let mut directories = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .collect::<Vec<_>>();
        directories.sort_by(|left, right| {
            path_depth(&right.manifest_path).cmp(&path_depth(&left.manifest_path))
        });
        for entry in directories {
            chmod(&entry.path, entry.mode.expect("directory entry has mode"))?;
        }

        Ok(())
    }

    fn validate_entries(&self) -> Result<Vec<ResolvedEntry>, ExecutorErrorReport> {
        self.entries
            .iter()
            .map(|entry| self.validate_entry(entry))
            .collect()
    }

    fn validate_entry(&self, entry: &FsTreeEntry) -> Result<ResolvedEntry, ExecutorErrorReport> {
        let path = entry_path(&self.target_inside, entry.path())?;
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                report_io(
                    "missing",
                    &path,
                    format!("missing fs-tree entry '{}'", path.display()),
                    error,
                )
            } else {
                report_io(
                    "stat",
                    &path,
                    format!("failed to inspect fs-tree entry '{}'", path.display()),
                    error,
                )
            }
        })?;

        let actual_kind = EntryKind::from_metadata(&metadata);
        let expected_kind = EntryKind::from_entry(entry);
        if actual_kind != Some(expected_kind) {
            return Err(report(
                "kind",
                &path,
                format!(
                    "fs-tree entry '{}' has kind {}, expected {}",
                    path.display(),
                    actual_kind.map_or("other", EntryKind::as_str),
                    expected_kind.as_str()
                ),
                None,
            ));
        }

        let (uid, gid, mode) = entry_attrs(entry);
        Ok(ResolvedEntry {
            manifest_path: entry.path().to_string(),
            path,
            kind: expected_kind,
            uid,
            gid,
            mode,
        })
    }
}

impl Executor for OwnershipExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        match self.apply() {
            Ok(()) => std::process::exit(0),
            Err(report) => {
                write_executor_error_report(&self.error_log_inside, &report)?;
                Err(ExecutorError::Other(report.to_string()))
            }
        }
    }
}

#[derive(Debug)]
struct ResolvedEntry {
    manifest_path: String,
    path: PathBuf,
    kind: EntryKind,
    uid: u32,
    gid: u32,
    mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
}

impl EntryKind {
    fn from_entry(entry: &FsTreeEntry) -> Self {
        match entry {
            FsTreeEntry::File { .. } => Self::File,
            FsTreeEntry::Directory { .. } => Self::Directory,
            FsTreeEntry::Symlink { .. } => Self::Symlink,
        }
    }

    fn from_metadata(metadata: &fs::Metadata) -> Option<Self> {
        let file_type = metadata.file_type();
        if file_type.is_file() {
            Some(Self::File)
        } else if file_type.is_dir() {
            Some(Self::Directory)
        } else if file_type.is_symlink() {
            Some(Self::Symlink)
        } else {
            None
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
        }
    }
}

fn entry_attrs(entry: &FsTreeEntry) -> (u32, u32, Option<u32>) {
    match entry {
        FsTreeEntry::File { uid, gid, mode, .. }
        | FsTreeEntry::Directory { uid, gid, mode, .. } => (*uid, *gid, Some(*mode)),
        FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid, None),
    }
}

fn entry_path(target_inside: &Path, manifest_path: &str) -> Result<PathBuf, ExecutorErrorReport> {
    validate_manifest_path(target_inside, manifest_path)?;
    let path = if manifest_path.is_empty() {
        target_inside.to_path_buf()
    } else {
        target_inside.join(manifest_path)
    };

    if path.starts_with(target_inside) {
        Ok(path)
    } else {
        Err(report(
            "path",
            &path,
            format!(
                "fs-tree entry path '{}' escapes target '{}'",
                manifest_path,
                target_inside.display()
            ),
            None,
        ))
    }
}

fn validate_manifest_path(
    target_inside: &Path,
    manifest_path: &str,
) -> Result<(), ExecutorErrorReport> {
    let display_path = if manifest_path.is_empty() {
        target_inside.to_path_buf()
    } else {
        target_inside.join(manifest_path)
    };

    if Path::new(manifest_path).is_absolute() {
        return Err(report(
            "path",
            &display_path,
            format!("fs-tree entry path '{manifest_path}' must be relative"),
            None,
        ));
    }

    for component in Path::new(manifest_path).components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(report(
                    "path",
                    &display_path,
                    format!("fs-tree entry path '{manifest_path}' contains unsafe component"),
                    None,
                ));
            }
        }
    }

    Ok(())
}

fn chown_if_needed(path: &Path, uid: u32, gid: u32) -> Result<(), ExecutorErrorReport> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        report_io(
            "stat",
            path,
            format!("failed to inspect fs-tree entry '{}'", path.display()),
            error,
        )
    })?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }

    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(|error| {
        report_errno(
            "chown",
            path,
            format!("failed to chown '{}': {error}", path.display()),
            error as i32,
        )
    })
}

fn lchown_if_needed(path: &Path, uid: u32, gid: u32) -> Result<(), ExecutorErrorReport> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        report_io(
            "stat",
            path,
            format!("failed to inspect fs-tree entry '{}'", path.display()),
            error,
        )
    })?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|error| {
        report(
            "path",
            path,
            format!(
                "failed to convert path '{}' for lchown: {error}",
                path.display()
            ),
            None,
        )
    })?;
    let result = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        Err(report_io(
            "lchown",
            path,
            format!("failed to lchown '{}': {error}", path.display()),
            error,
        ))
    }
}

fn chmod(path: &Path, mode: u32) -> Result<(), ExecutorErrorReport> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        report_io(
            "chmod",
            path,
            format!("failed to chmod '{}': {error}", path.display()),
            error,
        )
    })
}

fn path_depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.split('/').count()
    }
}

fn report(
    kind: impl Into<String>,
    path: &Path,
    message: String,
    errno: Option<i32>,
) -> ExecutorErrorReport {
    ExecutorErrorReport {
        kind: kind.into(),
        path: path.display().to_string(),
        message,
        errno,
    }
}

fn report_io(
    kind: impl Into<String>,
    path: &Path,
    message: String,
    error: io::Error,
) -> ExecutorErrorReport {
    report(
        kind,
        path,
        format!("{message}: {error}"),
        error.raw_os_error(),
    )
}

fn report_errno(
    kind: impl Into<String>,
    path: &Path,
    message: String,
    errno: i32,
) -> ExecutorErrorReport {
    report(kind, path, message, Some(errno))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_executor_error_report;
    use libcontainer::oci_spec::runtime::Spec;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use tempfile::tempdir;

    #[test]
    fn apply_sets_modes_for_current_owner_tree() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::create_dir(target.join("dir")).unwrap();
        fs::create_dir(target.join("dir/nested")).unwrap();
        fs::write(target.join("file"), b"file").unwrap();
        symlink("file", target.join("link")).unwrap();

        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::directory("dir", owner.0, owner.1, 0o700),
            FsTreeEntry::directory("dir/nested", owner.0, owner.1, 0o500),
            FsTreeEntry::file("file", owner.0, owner.1, 0o640),
            FsTreeEntry::symlink("link", owner.0, owner.1),
        ])
        .unwrap();

        OwnershipExecutor::with_paths(&manifest, target.clone(), temp.path().join("error.json"))
            .apply()
            .unwrap();

        assert_mode(&target, 0o755);
        assert_mode(target.join("dir"), 0o700);
        assert_mode(target.join("dir/nested"), 0o500);
        assert_mode(target.join("file"), 0o640);

        let link = fs::symlink_metadata(target.join("link")).unwrap();
        assert!(link.file_type().is_symlink());
        assert_eq!(link.uid(), owner.0);
        assert_eq!(link.gid(), owner.1);
    }

    #[test]
    fn exec_writes_missing_path_report() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let error_log = temp.path().join("error.json");
        fs::create_dir(&target).unwrap();
        fs::write(&error_log, b"").unwrap();
        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::file("missing", owner.0, owner.1, 0o644),
        ])
        .unwrap();

        let executor = OwnershipExecutor::with_paths(&manifest, target, error_log.clone());
        let error = executor
            .exec(&Spec::default())
            .expect_err("missing entry should fail before process exit");

        let report = read_executor_error_report(&error_log).unwrap().unwrap();
        assert_eq!(report.kind, "missing");
        assert!(report.path.ends_with("/target/missing"));
        assert!(report.message.contains("missing fs-tree entry"));
        assert!(error.to_string().contains("missing error"));
    }

    #[test]
    fn exec_writes_kind_mismatch_report() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let error_log = temp.path().join("error.json");
        fs::create_dir(&target).unwrap();
        fs::create_dir(target.join("entry")).unwrap();
        fs::write(&error_log, b"").unwrap();
        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::file("entry", owner.0, owner.1, 0o644),
        ])
        .unwrap();

        let executor = OwnershipExecutor::with_paths(&manifest, target, error_log.clone());
        executor
            .exec(&Spec::default())
            .expect_err("kind mismatch should fail before process exit");

        let report = read_executor_error_report(&error_log).unwrap().unwrap();
        assert_eq!(report.kind, "kind");
        assert!(report.path.ends_with("/target/entry"));
        assert!(report.message.contains("expected file"));
    }

    #[test]
    fn apply_rejects_unsafe_manifest_path() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let owner = current_owner();
        let executor = OwnershipExecutor {
            target_inside: target.clone(),
            entries: vec![FsTreeEntry::File {
                path: "../escape".to_string(),
                uid: owner.0,
                gid: owner.1,
                mode: 0o644,
            }],
            error_log_inside: temp.path().join("error.json"),
        };

        let report = executor.apply().unwrap_err();

        assert_eq!(report.kind, "path");
        assert!(report.message.contains("unsafe component"));
    }

    #[test]
    fn apply_reports_stat_errors() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, b"not a directory").unwrap();
        let owner = current_owner();
        let executor = OwnershipExecutor {
            target_inside: target,
            entries: vec![FsTreeEntry::File {
                path: "child".to_string(),
                uid: owner.0,
                gid: owner.1,
                mode: 0o644,
            }],
            error_log_inside: temp.path().join("error.json"),
        };

        let report = executor.apply().unwrap_err();

        assert_eq!(report.kind, "stat");
        assert!(report.message.contains("failed to inspect fs-tree entry"));
    }

    fn current_owner() -> (u32, u32) {
        (unsafe { libc::geteuid() }, unsafe { libc::getegid() })
    }

    fn assert_mode(path: impl AsRef<Path>, mode: u32) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, mode);
    }
}
