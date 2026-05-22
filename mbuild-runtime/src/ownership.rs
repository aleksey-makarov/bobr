//! Public ownership materialization entrypoints.

use crate::{
    error::RuntimeError,
    executor::{
        ExecutorErrorReport, ExecutorResultTimings, read_executor_result_report_with_timings,
        write_executor_error_report, write_executor_result_report_with_timings,
    },
    idmap::MbuildIdmap,
    local_ownership::{preflight_local_ownership_runtime, run_local_ownership},
};
use fsobj_hash::{ObjectHash, hash_fs_tree_object_with_extra_files, hash_path};
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
use std::time::Instant;

/// Result of ownership materialization plus object hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipHashResult {
    /// Hash of the materialized object.
    pub object_hash: ObjectHash,
    /// Timings reported by the ownership helper.
    pub timings: OwnershipTimings,
}

/// Phase timings reported by the ownership helper.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnershipTimings {
    /// Total helper-side time.
    pub total_ms: u128,
    /// Time spent resolving and validating manifest entries.
    pub validate_entries_ms: u128,
    /// Time spent applying file and directory ownership.
    pub chown_ms: u128,
    /// Time spent applying symlink ownership.
    pub lchown_ms: u128,
    /// Time spent applying file modes.
    pub chmod_files_ms: u128,
    /// Time spent applying directory modes.
    pub chmod_dirs_ms: u128,
    /// Time spent validating the materialized entries after mutation.
    pub validate_applied_ms: u128,
    /// Time spent serializing the fs-tree manifest for hashing.
    pub manifest_serialize_ms: u128,
    /// Time spent hashing the target tree or fs-tree object.
    pub hash_ms: u128,
}

impl From<ExecutorResultTimings> for OwnershipTimings {
    fn from(timings: ExecutorResultTimings) -> Self {
        Self {
            total_ms: timings.total_ms,
            validate_entries_ms: timings.validate_entries_ms,
            chown_ms: timings.chown_ms,
            lchown_ms: timings.lchown_ms,
            chmod_files_ms: timings.chmod_files_ms,
            chmod_dirs_ms: timings.chmod_dirs_ms,
            validate_applied_ms: timings.validate_applied_ms,
            manifest_serialize_ms: timings.manifest_serialize_ms,
            hash_ms: timings.hash_ms,
        }
    }
}

impl From<OwnershipTimings> for ExecutorResultTimings {
    fn from(timings: OwnershipTimings) -> Self {
        Self {
            total_ms: timings.total_ms,
            validate_entries_ms: timings.validate_entries_ms,
            chown_ms: timings.chown_ms,
            lchown_ms: timings.lchown_ms,
            chmod_files_ms: timings.chmod_files_ms,
            chmod_dirs_ms: timings.chmod_dirs_ms,
            validate_applied_ms: timings.validate_applied_ms,
            manifest_serialize_ms: timings.manifest_serialize_ms,
            hash_ms: timings.hash_ms,
        }
    }
}

/// Apply fs-tree owners and modes to an existing directory tree.
///
/// `manifest` describes paths relative to `target_root` using logical uid/gid
/// values. The runtime validates that every logical owner is representable by
/// `idmap`, starts an internal ownership helper in the mapped user namespace,
/// applies file, directory, and symlink ownership, then applies file and
/// directory modes.
///
/// `workspace` must exist and is used for temporary runtime bundle and state
/// directories. Runtime-owned temporary directories are removed before the
/// function returns.
pub fn apply_ownership_batch(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    run_ownership_batch(target_root, manifest, idmap, workspace, None)?;
    Ok(())
}

/// Apply fs-tree owners and modes, then compute the fs object hash inside the
/// same user namespace.
///
/// The returned hash is computed after ownership and mode materialization. This
/// lets callers publish target-owned trees without requiring the host user to
/// recursively read the materialized root.
pub fn apply_ownership_batch_and_hash(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<ObjectHash, RuntimeError> {
    Ok(
        apply_ownership_batch_and_hash_with_timings(target_root, manifest, idmap, workspace)?
            .object_hash,
    )
}

/// Apply fs-tree owners and modes, compute the fs object hash, and return
/// helper-side phase timings.
pub fn apply_ownership_batch_and_hash_with_timings(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<OwnershipHashResult, RuntimeError> {
    let bundle = run_ownership_batch(
        target_root,
        manifest,
        idmap,
        workspace,
        Some(HashReport::TargetRoot),
    )?;
    read_ownership_hash_result(bundle.result_report())
}

/// Apply fs-tree owners and modes, then compute the hash of a synthetic
/// fs-tree object made from `manifest.jsonl` and `target_root`.
///
/// The returned hash matches the object shape created by mbuild fs-tree
/// builders: a directory with canonical `manifest.jsonl` plus `root/`.
/// Hashing happens inside the ownership user namespace so callers can publish
/// trees that are not recursively readable by the host user.
pub fn apply_ownership_batch_and_hash_fs_tree_object(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<ObjectHash, RuntimeError> {
    Ok(apply_ownership_batch_and_hash_fs_tree_object_with_timings(
        target_root,
        manifest,
        idmap,
        workspace,
    )?
    .object_hash)
}

/// Apply fs-tree owners and modes, compute the synthetic fs-tree object hash,
/// and return helper-side phase timings.
pub fn apply_ownership_batch_and_hash_fs_tree_object_with_timings(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<OwnershipHashResult, RuntimeError> {
    apply_selected_ownership_batch_and_hash_fs_tree_object_with_timings(
        target_root,
        manifest,
        manifest,
        Vec::new(),
        idmap,
        workspace,
    )
}

/// Apply fs-tree owners and modes, then compute the hash of a synthetic
/// fs-tree object with additional top-level non-executable metadata files.
///
/// `extra_files` contains `(name, bytes)` pairs. Names are fsobj directory entry
/// names, not paths; callers use this for metadata such as `oci-config.json`
/// that participates in object identity but is not part of `root/`.
pub fn apply_ownership_batch_and_hash_fs_tree_object_with_extra_files(
    target_root: &Path,
    manifest: &FsTreeManifest,
    extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<ObjectHash, RuntimeError> {
    Ok(
        apply_selected_ownership_batch_and_hash_fs_tree_object_with_timings(
            target_root,
            manifest,
            manifest,
            extra_files,
            idmap,
            workspace,
        )?
        .object_hash,
    )
}

/// Apply fs-tree owners and modes for selected entries, then compute the hash
/// of a synthetic fs-tree object made from `object_manifest` and `target_root`.
///
/// Entries omitted from `materialize_manifest` are not mutated; callers must
/// validate them before calling this function. This is intended for composed
/// trees where some files are immutable hardlinks to store inputs and must not
/// be chowned or chmodded.
pub fn apply_selected_ownership_batch_and_hash_fs_tree_object(
    target_root: &Path,
    materialize_manifest: &FsTreeManifest,
    object_manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<ObjectHash, RuntimeError> {
    Ok(
        apply_selected_ownership_batch_and_hash_fs_tree_object_with_timings(
            target_root,
            materialize_manifest,
            object_manifest,
            Vec::new(),
            idmap,
            workspace,
        )?
        .object_hash,
    )
}

/// Apply selected fs-tree owners and modes, compute the synthetic fs-tree
/// object hash, and return helper-side phase timings.
pub fn apply_selected_ownership_batch_and_hash_fs_tree_object_with_timings(
    target_root: &Path,
    materialize_manifest: &FsTreeManifest,
    object_manifest: &FsTreeManifest,
    extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<OwnershipHashResult, RuntimeError> {
    let bundle = run_ownership_batch(
        target_root,
        materialize_manifest,
        idmap,
        workspace,
        Some(HashReport::FsTreeObject {
            manifest: object_manifest.clone(),
            extra_files,
        }),
    )?;
    read_ownership_hash_result(bundle.result_report())
}

fn read_ownership_hash_result(path: &Path) -> Result<OwnershipHashResult, RuntimeError> {
    let result = read_executor_result_report_with_timings(path)?.ok_or_else(|| {
        RuntimeError::Executor(format!(
            "executor result report '{}' is empty",
            path.display()
        ))
    })?;
    Ok(OwnershipHashResult {
        object_hash: result.object_hash,
        timings: result.timings.map(Into::into).unwrap_or_default(),
    })
}

fn run_ownership_batch(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
    hash_report: Option<HashReport>,
) -> Result<crate::local_ownership::LocalOwnershipRun, RuntimeError> {
    require_directory(target_root, "ownership target root")?;
    require_directory(workspace, "ownership workspace")?;
    precheck_manifest_owners(manifest, idmap)?;
    preflight_local_ownership_runtime(idmap)?;

    run_local_ownership(target_root, manifest, idmap, workspace, hash_report)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HashReport {
    TargetRoot,
    FsTreeObject {
        manifest: FsTreeManifest,
        extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct OwnershipExecutor {
    target_inside: PathBuf,
    target_display: PathBuf,
    entries: Vec<FsTreeEntry>,
    error_log_inside: PathBuf,
    result_log_inside: Option<PathBuf>,
    hash_report: Option<HashReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HashComputation {
    object_hash: ObjectHash,
    manifest_serialize_ms: u128,
    hash_ms: u128,
}

impl OwnershipExecutor {
    #[cfg(test)]
    pub(crate) fn with_paths(
        manifest: &FsTreeManifest,
        target_inside: PathBuf,
        error_log_inside: PathBuf,
    ) -> Self {
        Self::with_paths_and_result(manifest, target_inside, error_log_inside, None, None)
    }

    #[cfg(test)]
    pub(crate) fn with_paths_and_result(
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

    pub(crate) fn with_paths_display_and_result(
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

    pub(crate) fn apply(&self) -> Result<Option<OwnershipHashResult>, ExecutorErrorReport> {
        let total_start = Instant::now();
        let mut timings = OwnershipTimings::default();

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

        let step_start = Instant::now();
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
        }
        timings.chmod_dirs_ms = elapsed_ms(step_start);

        let step_start = Instant::now();
        Self::validate_applied_entries(&entries)?;
        timings.validate_applied_ms = elapsed_ms(step_start);
        if let Some(report) = self.hash_report.as_ref() {
            let hash_result = self.hash_result(report)?;
            timings.manifest_serialize_ms = hash_result.manifest_serialize_ms;
            timings.hash_ms = hash_result.hash_ms;
            timings.total_ms = elapsed_ms(total_start);
            Ok(Some(OwnershipHashResult {
                object_hash: hash_result.object_hash,
                timings,
            }))
        } else {
            Ok(None)
        }
    }

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

    fn validate_entries(&self) -> Result<Vec<ResolvedEntry>, ExecutorErrorReport> {
        self.entries
            .iter()
            .map(|entry| self.validate_entry(entry))
            .collect()
    }

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

    fn validate_applied_entries(entries: &[ResolvedEntry]) -> Result<(), ExecutorErrorReport> {
        for entry in entries {
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
        }

        Ok(())
    }

    fn report_path(&self, path: &Path) -> PathBuf {
        let relative = path.strip_prefix(&self.target_inside).unwrap_or(path);
        if relative.as_os_str().is_empty() {
            self.target_display.clone()
        } else {
            self.target_display.join(relative)
        }
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
            Ok(result) => {
                if let (Some(path), Some(result)) = (&self.result_log_inside, result) {
                    write_executor_result_report_with_timings(
                        path,
                        result.object_hash,
                        Some(result.timings.into()),
                    )?;
                }
                std::process::exit(0)
            }
            Err(report) => {
                write_executor_error_report(&self.error_log_inside, &report)?;
                Err(ExecutorError::Other(report.to_string()))
            }
        }
    }
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

#[derive(Debug)]
struct ResolvedEntry {
    manifest_path: String,
    path: PathBuf,
    report_path: PathBuf,
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

fn require_directory(path: &Path, label: &str) -> Result<(), RuntimeError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "{label} '{}' must exist and be a directory",
            path.display()
        )))
    }
}

fn precheck_manifest_owners(
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    for entry in manifest.entries() {
        let (uid, gid, _) = entry_attrs(entry);
        idmap.physical_uid(uid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry_label(entry)))
        })?;
        idmap.physical_gid(gid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry_label(entry)))
        })?;
    }
    Ok(())
}

fn entry_label(entry: &FsTreeEntry) -> &str {
    if entry.path().is_empty() {
        "."
    } else {
        entry.path()
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
    use crate::executor::read_executor_error_report;
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

    #[test]
    fn require_directory_rejects_missing_and_non_directory_paths() {
        let temp = tempdir().unwrap();
        let missing = temp.path().join("missing");
        let file = temp.path().join("file");
        fs::write(&file, b"not a directory").unwrap();

        let missing_error = require_directory(&missing, "ownership target root").unwrap_err();
        let file_error = require_directory(&file, "ownership workspace").unwrap_err();

        assert!(matches!(missing_error, RuntimeError::InvalidInput(_)));
        assert!(missing_error.to_string().contains("ownership target root"));
        assert!(matches!(file_error, RuntimeError::InvalidInput(_)));
        assert!(file_error.to_string().contains("ownership workspace"));
    }

    #[test]
    fn apply_ownership_batch_rejects_invalid_target_root_before_container_setup() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let missing_target = temp.path().join("missing-target");
        let manifest = root_only_manifest();

        let error = apply_ownership_batch(&missing_target, &manifest, &test_idmap(), &workspace)
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("ownership target root"));
    }

    #[test]
    fn apply_ownership_batch_rejects_invalid_workspace_before_container_setup() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let workspace_file = temp.path().join("workspace-file");
        fs::create_dir(&target).unwrap();
        fs::write(&workspace_file, b"not a directory").unwrap();
        let manifest = root_only_manifest();

        let error =
            apply_ownership_batch(&target, &manifest, &test_idmap(), &workspace_file).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("ownership workspace"));
    }

    #[test]
    fn precheck_manifest_owners_accepts_mapped_logical_ids() {
        let idmap = test_idmap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::file("file", 1, 1, 0o644),
            FsTreeEntry::symlink("link", 1, 1, "target"),
        ])
        .unwrap();

        precheck_manifest_owners(&manifest, &idmap).unwrap();
    }

    #[test]
    fn precheck_manifest_owners_rejects_out_of_range_uid_with_entry_path() {
        let idmap = test_idmap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::file("file", 2, 1, 0o644),
        ])
        .unwrap();

        let error = precheck_manifest_owners(&manifest, &idmap).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("fs-tree entry 'file'"));
        assert!(error.to_string().contains("logical uid 2"));
    }

    #[test]
    fn precheck_manifest_owners_rejects_out_of_range_gid_with_entry_path() {
        let idmap = test_idmap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("dir", 1, 2, 0o755),
        ])
        .unwrap();

        let error = precheck_manifest_owners(&manifest, &idmap).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("fs-tree entry 'dir'"));
        assert!(error.to_string().contains("logical gid 2"));
    }

    fn current_owner() -> (u32, u32) {
        (unsafe { libc::geteuid() }, unsafe { libc::getegid() })
    }

    fn test_idmap() -> MbuildIdmap {
        MbuildIdmap::for_tests(1000, 1001, 100000, 1, 200000, 1)
    }

    fn root_only_manifest() -> FsTreeManifest {
        FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap()
    }

    fn assert_mode(path: impl AsRef<Path>, mode: u32) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, mode);
    }
}
