//! Helper-side implementation of the `ownership` operation.
//!
//! This code runs in the helper process after the parent runtime has created
//! the user namespace and configured uid/gid maps. The parent owns process
//! lifecycle and passes only a JSON config file; this module owns interpreting
//! that config, mutating the target tree, and writing structured report files.

use fsobj_hash::{ObjectHash, hash_fs_tree_object_with_extra_files, hash_path};
use mbuild_core::runtime_helper_protocol::{
    ExecutorErrorReport, ExecutorResultTimings, OwnershipHelperConfig, OwnershipHelperHashReport,
    write_executor_error_report, write_executor_result_report_with_timings,
};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use nix::unistd::{Gid, Uid, chown};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

/// Run the ownership operation from a JSON config file path.
///
/// This is the operation entrypoint used by the helper CLI command
/// `ownership --config PATH`.
pub(crate) fn run_config_path(path: &Path) -> Result<(), String> {
    let config = read_config(path)?;
    run_config(config)
}

/// Read and decode the wire-level config written by the parent runtime.
fn read_config(path: &Path) -> Result<OwnershipHelperConfig, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read helper config '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to parse helper config '{}': {error}",
            path.display()
        )
    })
}

/// Convert a wire-level ownership config into helper execution state and run it.
///
/// The config paths are already helper-visible paths. `target_root` is the
/// actual filesystem root to mutate, while report paths are files the parent
/// will inspect after the helper exits. Display paths in structured errors are
/// intentionally rooted at `/target` instead of exposing host paths.
fn run_config(config: OwnershipHelperConfig) -> Result<(), String> {
    let manifest = parse_manifest("manifest", &config.manifest, &config.error_report)?;
    let hash_report = match config.hash_report {
        Some(OwnershipHelperHashReport::TargetRoot) => Some(HashReport::TargetRoot),
        Some(OwnershipHelperHashReport::FsTreeObject {
            manifest,
            extra_files,
        }) => Some(HashReport::FsTreeObject {
            manifest: parse_manifest("hash manifest", &manifest, &config.error_report)?,
            extra_files,
        }),
        None => None,
    };

    let executor = OwnershipExecutor::with_paths_display_and_result(
        &manifest,
        config.target_root,
        PathBuf::from("/target"),
        config.error_report.clone(),
        config.result_report.clone(),
        hash_report,
    );
    run_executor(&executor)
}

/// Parse a canonical fs-tree manifest and write a structured report on failure.
///
/// Manifest parse errors happen before an [`OwnershipExecutor`] exists, so this
/// function writes directly to the configured error report path.
fn parse_manifest(label: &str, text: &str, error_report: &Path) -> Result<FsTreeManifest, String> {
    FsTreeManifest::parse_canonical_bytes(text.as_bytes()).map_err(|error| {
        let report = ExecutorErrorReport {
            kind: "manifest".to_string(),
            path: error_report.display().to_string(),
            message: format!("failed to parse {label}: {error}"),
            errno: None,
        };
        let _ = write_executor_error_report(error_report, &report);
        report.to_string()
    })
}

/// Apply an executor and translate its outcome into the report-file protocol.
///
/// Successful ownership-only runs write no result file. Successful hash-producing
/// runs write `result_log_inside` when the parent requested one. Failures always
/// try to write `error_log_inside` before returning a textual error for stderr.
fn run_executor(executor: &OwnershipExecutor) -> Result<(), String> {
    match executor.apply() {
        Ok(result) => {
            if let (Some(path), Some(result)) = (&executor.result_log_inside, result) {
                write_executor_result_report_with_timings(
                    path,
                    result.object_hash,
                    Some(result.timings),
                )
                .map_err(|error| {
                    format!(
                        "failed to write executor result report '{}': {error}",
                        path.display()
                    )
                })?;
            }
            Ok(())
        }
        Err(report) => {
            write_executor_error_report(&executor.error_log_inside, &report).map_err(|error| {
                format!(
                    "failed to write executor error report '{}': {error}; original error: {report}",
                    executor.error_log_inside.display()
                )
            })?;
            Err(report.to_string())
        }
    }
}

/// Helper-local hash request derived from the wire protocol.
///
/// Hashing is always performed after ownership and mode materialization. The
/// parent asks for a hash by setting this field and provides `result_report`
/// when it needs the hash returned across the process boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HashReport {
    /// Hash the materialized target root directly.
    TargetRoot,
    /// Hash the fs-tree object shape `{ manifest.jsonl, root/, ...extra }`.
    FsTreeObject {
        /// Manifest that defines the synthetic fs-tree object, not necessarily
        /// the same manifest used for materialization.
        manifest: FsTreeManifest,
        /// Extra top-level files included in the synthetic object hash.
        extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

/// Ownership materialization executor for one helper invocation.
///
/// The executor stores immutable execution state. It validates every manifest
/// entry against the current target tree, applies ownership first, then modes,
/// validates the final tree, and optionally computes a hash result.
#[derive(Debug, Clone)]
struct OwnershipExecutor {
    /// Real helper-visible path to the target root to mutate and hash.
    target_inside: PathBuf,
    /// Stable display root used in structured reports instead of host paths.
    target_display: PathBuf,
    /// Canonical manifest entries to materialize.
    entries: Vec<FsTreeEntry>,
    /// Helper-visible path for the structured failure report.
    error_log_inside: PathBuf,
    /// Optional helper-visible path for the structured success report.
    result_log_inside: Option<PathBuf>,
    /// Optional post-materialization hash request.
    hash_report: Option<HashReport>,
}

/// Successful executor result that can be serialized to `result_log_inside`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnershipHelperResult {
    /// Object hash computed after materialization.
    object_hash: ObjectHash,
    /// Helper-side phase timings.
    timings: ExecutorResultTimings,
}

/// Hash result plus timing components folded into the final result timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HashComputation {
    /// Computed object hash.
    object_hash: ObjectHash,
    /// Time spent serializing the manifest used for fs-tree object hashing.
    manifest_serialize_ms: u128,
    /// Time spent computing the target-root or fs-tree-object hash.
    hash_ms: u128,
}

impl OwnershipExecutor {
    #[cfg(test)]
    fn with_paths(
        manifest: &FsTreeManifest,
        target_inside: PathBuf,
        error_log_inside: PathBuf,
    ) -> Self {
        Self::with_paths_and_result(manifest, target_inside, error_log_inside, None, None)
    }

    #[cfg(test)]
    fn with_paths_and_result(
        manifest: &FsTreeManifest,
        target_inside: PathBuf,
        error_log_inside: PathBuf,
        result_log_inside: Option<PathBuf>,
        hash_report: Option<HashReport>,
    ) -> Self {
        Self::with_paths_display_and_result(
            manifest,
            target_inside.clone(),
            target_inside,
            error_log_inside,
            result_log_inside,
            hash_report,
        )
    }

    /// Build an executor from parsed config fields.
    ///
    /// `target_inside` is the real path used for syscalls. `target_display` is
    /// the report path prefix visible to users and must not be used for syscalls.
    fn with_paths_display_and_result(
        manifest: &FsTreeManifest,
        target_inside: PathBuf,
        target_display: PathBuf,
        error_log_inside: PathBuf,
        result_log_inside: Option<PathBuf>,
        hash_report: Option<HashReport>,
    ) -> Self {
        Self {
            target_inside,
            target_display,
            entries: manifest.entries().to_vec(),
            error_log_inside,
            result_log_inside,
            hash_report,
        }
    }

    /// Materialize ownership/modes and optionally compute a result hash.
    ///
    /// Ownership changes happen before any mode changes so restrictive modes in
    /// the manifest cannot prevent later `chown`/`lchown` calls. File modes are
    /// then applied before directory modes. Directory modes are applied
    /// deepest-first so chmodding a parent directory cannot remove access needed
    /// to finish nested entries.
    fn apply(&self) -> Result<Option<OwnershipHelperResult>, ExecutorErrorReport> {
        let total_start = Instant::now();
        let mut timings = ExecutorResultTimings::default();

        let step_start = Instant::now();
        let entries = self.validate_entries()?;
        timings.validate_entries_ms = elapsed_ms(step_start);

        let step_start = Instant::now();
        for entry in &entries {
            match entry.kind {
                EntryKind::File | EntryKind::Directory => {
                    chown_if_needed(&entry.path, &entry.report_path, entry.uid, entry.gid)?;
                }
                EntryKind::Symlink => {}
            }
        }
        timings.chown_ms = elapsed_ms(step_start);

        let step_start = Instant::now();
        for entry in &entries {
            if entry.kind == EntryKind::Symlink {
                lchown_if_needed(&entry.path, &entry.report_path, entry.uid, entry.gid)?;
            }
        }
        timings.lchown_ms = elapsed_ms(step_start);

        let step_start = Instant::now();
        for entry in &entries {
            if entry.kind == EntryKind::File {
                chmod(
                    &entry.path,
                    &entry.report_path,
                    entry.mode.expect("file entry has mode"),
                )?;
            }
        }
        timings.chmod_files_ms = elapsed_ms(step_start);

        let validate_start = Instant::now();
        for entry in entries
            .iter()
            .filter(|entry| entry.kind != EntryKind::Directory)
        {
            Self::validate_applied_entry(entry)?;
        }
        timings.validate_applied_ms = elapsed_ms(validate_start);

        let step_start = Instant::now();
        // Parent directory modes can make children unreachable. Defer every
        // directory chmod until non-directories are done and validated, then
        // walk from leaves to root. Each directory is validated immediately
        // after its chmod while its parent directories are still accessible.
        let mut directories = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .collect::<Vec<_>>();
        directories.sort_by(|left, right| {
            path_depth(&right.manifest_path).cmp(&path_depth(&left.manifest_path))
        });
        for entry in directories {
            chmod(
                &entry.path,
                &entry.report_path,
                entry.mode.expect("directory entry has mode"),
            )?;
            let validate_start = Instant::now();
            Self::validate_applied_entry(entry)?;
            timings.validate_applied_ms += elapsed_ms(validate_start);
        }
        timings.chmod_dirs_ms = elapsed_ms(step_start);

        if let Some(report) = self.hash_report.as_ref() {
            let hash_result = self.hash_result(report)?;
            timings.manifest_serialize_ms = hash_result.manifest_serialize_ms;
            timings.hash_ms = hash_result.hash_ms;
            timings.total_ms = elapsed_ms(total_start);
            Ok(Some(OwnershipHelperResult {
                object_hash: hash_result.object_hash,
                timings,
            }))
        } else {
            Ok(None)
        }
    }

    /// Compute the requested post-materialization hash.
    fn hash_result(
        &self,
        report_kind: &HashReport,
    ) -> Result<HashComputation, ExecutorErrorReport> {
        match report_kind {
            HashReport::TargetRoot => {
                let hash_start = Instant::now();
                let object_hash = hash_path(&self.target_inside).map_err(|error| {
                    report(
                        "hash",
                        &self.target_display,
                        format!(
                            "failed to hash fs-tree target '{}': {error}",
                            self.target_display.display()
                        ),
                        None,
                    )
                })?;
                Ok(HashComputation {
                    object_hash,
                    manifest_serialize_ms: 0,
                    hash_ms: elapsed_ms(hash_start),
                })
            }
            HashReport::FsTreeObject {
                manifest,
                extra_files,
            } => {
                let manifest_start = Instant::now();
                let manifest_bytes = manifest.to_canonical_bytes().map_err(|error| {
                    report(
                        "hash",
                        &self.target_display,
                        format!("failed to serialize fs-tree manifest for hashing: {error}"),
                        None,
                    )
                })?;
                let manifest_serialize_ms = elapsed_ms(manifest_start);
                let hash_start = Instant::now();
                let extra_file_refs = extra_files
                    .iter()
                    .map(|(name, content)| (name.as_slice(), content.as_slice()))
                    .collect::<Vec<_>>();
                let object_hash = hash_fs_tree_object_with_extra_files(
                    &manifest_bytes,
                    &self.target_inside,
                    &extra_file_refs,
                )
                .map_err(|error| {
                    report(
                        "hash",
                        &self.target_display,
                        format!(
                            "failed to hash fs-tree object rooted at '{}': {error}",
                            self.target_display.display()
                        ),
                        None,
                    )
                })?;
                Ok(HashComputation {
                    object_hash,
                    manifest_serialize_ms,
                    hash_ms: elapsed_ms(hash_start),
                })
            }
        }
    }

    /// Resolve every manifest entry to a concrete target path and metadata.
    fn validate_entries(&self) -> Result<Vec<ResolvedEntry>, ExecutorErrorReport> {
        self.entries
            .iter()
            .map(|entry| self.validate_entry(entry))
            .collect()
    }

    /// Validate one manifest entry before any mutation happens.
    ///
    /// This catches missing paths and kind mismatches early and records the
    /// report path that should be reused for later mutation/validation errors.
    fn validate_entry(&self, entry: &FsTreeEntry) -> Result<ResolvedEntry, ExecutorErrorReport> {
        let path = entry_path(&self.target_inside, entry.path())?;
        let report_path = self.report_path(&path);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                report_io(
                    "missing",
                    &report_path,
                    format!("missing fs-tree entry '{}'", report_path.display()),
                    error,
                )
            } else {
                report_io(
                    "stat",
                    &report_path,
                    format!(
                        "failed to inspect fs-tree entry '{}'",
                        report_path.display()
                    ),
                    error,
                )
            }
        })?;

        let actual_kind = EntryKind::from_metadata(&metadata);
        let expected_kind = EntryKind::from_entry(entry);
        if actual_kind != Some(expected_kind) {
            return Err(report(
                "kind",
                &report_path,
                format!(
                    "fs-tree entry '{}' has kind {}, expected {}",
                    report_path.display(),
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
            report_path,
            kind: expected_kind,
            uid,
            gid,
            mode,
        })
    }

    /// Re-check one materialized entry after its ownership and mode changes.
    ///
    /// This guards against partial application and catches unexpected filesystem
    /// changes during the helper run before reporting success to the parent.
    /// Callers must do this before applying restrictive modes to any parent
    /// directory needed to reach `entry`.
    fn validate_applied_entry(entry: &ResolvedEntry) -> Result<(), ExecutorErrorReport> {
        let metadata = fs::symlink_metadata(&entry.path).map_err(|error| {
            report_io(
                "stat",
                &entry.report_path,
                format!(
                    "failed to inspect fs-tree entry '{}'",
                    entry.report_path.display()
                ),
                error,
            )
        })?;

        let actual_kind = EntryKind::from_metadata(&metadata);
        if actual_kind != Some(entry.kind) {
            return Err(report(
                "kind",
                &entry.report_path,
                format!(
                    "fs-tree entry '{}' has kind {}, expected {} after ownership materialization",
                    entry.report_path.display(),
                    actual_kind.map_or("other", EntryKind::as_str),
                    entry.kind.as_str()
                ),
                None,
            ));
        }

        if metadata.uid() != entry.uid || metadata.gid() != entry.gid {
            return Err(report(
                "owner",
                &entry.report_path,
                format!(
                    "fs-tree entry '{}' has owner {}:{}, expected {}:{}",
                    entry.report_path.display(),
                    metadata.uid(),
                    metadata.gid(),
                    entry.uid,
                    entry.gid
                ),
                None,
            ));
        }

        if let Some(expected_mode) = entry.mode {
            let actual_mode = metadata.permissions().mode() & 0o7777;
            if actual_mode != expected_mode {
                return Err(report(
                    "mode",
                    &entry.report_path,
                    format!(
                        "fs-tree entry '{}' has mode {:o}, expected {:o}",
                        entry.report_path.display(),
                        actual_mode,
                        expected_mode
                    ),
                    None,
                ));
            }
        }

        Ok(())
    }

    /// Convert a real target path to the display path stored in reports.
    fn report_path(&self, path: &Path) -> PathBuf {
        let relative = path.strip_prefix(&self.target_inside).unwrap_or(path);
        if relative.as_os_str().is_empty() {
            self.target_display.clone()
        } else {
            self.target_display.join(relative)
        }
    }
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

/// Manifest entry resolved against the target tree.
#[derive(Debug)]
struct ResolvedEntry {
    /// Original manifest path, used for directory-depth ordering.
    manifest_path: String,
    /// Real helper-visible filesystem path.
    path: PathBuf,
    /// User-facing path written into structured reports.
    report_path: PathBuf,
    /// Expected filesystem kind.
    kind: EntryKind,
    /// Physical uid to apply and validate.
    uid: u32,
    /// Physical gid to apply and validate.
    gid: u32,
    /// File/directory mode to apply; symlinks have no mode in the manifest.
    mode: Option<u32>,
}

/// Filesystem entry kinds supported by fs-tree manifests.
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

/// Resolve a manifest-relative path under the target root.
///
/// The manifest path is validated before joining so absolute paths or `..`
/// components cannot escape the target tree.
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

/// Validate that a manifest path is a safe relative path.
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

/// Apply file or directory ownership when it differs from the expected owner.
fn chown_if_needed(
    path: &Path,
    report_path: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), ExecutorErrorReport> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        report_io(
            "stat",
            report_path,
            format!(
                "failed to inspect fs-tree entry '{}'",
                report_path.display()
            ),
            error,
        )
    })?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }

    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(|error| {
        report_errno(
            "chown",
            report_path,
            format!("failed to chown '{}': {error}", report_path.display()),
            error as i32,
        )
    })
}

/// Apply symlink ownership using `lchown` when it differs from expected owner.
fn lchown_if_needed(
    path: &Path,
    report_path: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), ExecutorErrorReport> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        report_io(
            "stat",
            report_path,
            format!(
                "failed to inspect fs-tree entry '{}'",
                report_path.display()
            ),
            error,
        )
    })?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|error| {
        report(
            "path",
            report_path,
            format!(
                "failed to convert path '{}' for lchown: {error}",
                report_path.display()
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
            report_path,
            format!("failed to lchown '{}': {error}", report_path.display()),
            error,
        ))
    }
}

/// Apply file or directory mode.
fn chmod(path: &Path, report_path: &Path, mode: u32) -> Result<(), ExecutorErrorReport> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        report_io(
            "chmod",
            report_path,
            format!("failed to chmod '{}': {error}", report_path.display()),
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
    use mbuild_core::runtime_helper_protocol::read_executor_error_report;
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
            FsTreeEntry::symlink("link", owner.0, owner.1, "target"),
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
    fn apply_preserves_special_mode_bits() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::create_dir(target.join("tmp")).unwrap();

        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::directory("tmp", owner.0, owner.1, 0o1777),
        ])
        .unwrap();

        OwnershipExecutor::with_paths(&manifest, target.clone(), temp.path().join("error.json"))
            .apply()
            .unwrap();

        assert_mode(target.join("tmp"), 0o1777);
    }

    #[test]
    fn apply_defers_restrictive_directory_modes_until_children_are_done() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::create_dir(target.join("locked")).unwrap();
        fs::write(target.join("locked/file"), b"file").unwrap();

        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::directory("locked", owner.0, owner.1, 0o000),
            FsTreeEntry::file("locked/file", owner.0, owner.1, 0o600),
        ])
        .unwrap();

        OwnershipExecutor::with_paths(&manifest, target.clone(), temp.path().join("error.json"))
            .apply()
            .unwrap();

        assert_mode(target.join("locked"), 0o000);
        fs::set_permissions(target.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            fs::symlink_metadata(target.join("locked/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }

    #[test]
    fn apply_returns_hash_when_result_path_is_requested() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("tool"), b"tool").unwrap();

        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::file("tool", owner.0, owner.1, 0o755),
        ])
        .unwrap();

        let got = OwnershipExecutor::with_paths_and_result(
            &manifest,
            target.clone(),
            temp.path().join("error.json"),
            Some(temp.path().join("result.json")),
            Some(HashReport::TargetRoot),
        )
        .apply()
        .unwrap();

        let got = got.unwrap();
        assert_eq!(got.object_hash, hash_path(&target).unwrap());
        assert!(got.timings.total_ms >= got.timings.hash_ms);
        assert_mode(target.join("tool"), 0o755);
    }

    #[test]
    fn apply_returns_fs_tree_object_hash_when_requested() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("tool"), b"tool").unwrap();

        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::file("tool", owner.0, owner.1, 0o755),
        ])
        .unwrap();

        let got = OwnershipExecutor::with_paths_and_result(
            &manifest,
            target.clone(),
            temp.path().join("error.json"),
            Some(temp.path().join("result.json")),
            Some(HashReport::FsTreeObject {
                manifest: manifest.clone(),
                extra_files: Vec::new(),
            }),
        )
        .apply()
        .unwrap();
        let manifest_bytes = manifest.to_canonical_bytes().unwrap();

        let got = got.unwrap();
        assert_eq!(
            got.object_hash,
            fsobj_hash::hash_fs_tree_object(&manifest_bytes, &target).unwrap()
        );
        assert_ne!(got.object_hash, hash_path(&target).unwrap());
        assert!(got.timings.total_ms >= got.timings.hash_ms);
        assert_mode(target.join("tool"), 0o755);
    }

    #[test]
    fn run_executor_writes_missing_path_report() {
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
        let error = run_executor(&executor).unwrap_err();

        let report = read_executor_error_report(&error_log).unwrap().unwrap();
        assert_eq!(report.kind, "missing");
        assert!(report.path.ends_with("/target/missing"));
        assert!(report.message.contains("missing fs-tree entry"));
        assert!(error.contains("missing error"));
    }

    #[test]
    fn run_executor_writes_kind_mismatch_report() {
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
        run_executor(&executor).unwrap_err();

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
            target_display: target.clone(),
            entries: vec![FsTreeEntry::file("../escape", owner.0, owner.1, 0o644)],
            error_log_inside: temp.path().join("error.json"),
            result_log_inside: None,
            hash_report: None,
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
            target_inside: target.clone(),
            target_display: target,
            entries: vec![FsTreeEntry::file("child", owner.0, owner.1, 0o644)],
            error_log_inside: temp.path().join("error.json"),
            result_log_inside: None,
            hash_report: None,
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
