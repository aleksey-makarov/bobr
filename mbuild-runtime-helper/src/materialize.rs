//! Helper-side implementation of the `fs-tree-materialize` operation.

use mbuild_core::runtime_helper_protocol::{
    ExecutorErrorReport, FsTreeArchiveEntrySource, FsTreeMaterializeHelperConfig,
    FsTreeMaterializeReport, write_executor_error_report, write_fs_tree_materialize_report,
};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use nix::unistd::{Gid, Uid, chown};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const MANIFEST_FILE_NAME: &str = "manifest.jsonl";
const ROOT_DIR_NAME: &str = "root";

/// Run the fs-tree materialize operation from a JSON config file path.
pub(crate) fn run_config_path(path: &Path) -> Result<(), String> {
    let config = read_config(path)?;
    run_config(config)
}

fn read_config(path: &Path) -> Result<FsTreeMaterializeHelperConfig, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read helper config '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to parse helper config '{}': {error}",
            path.display()
        )
    })
}

fn run_config(config: FsTreeMaterializeHelperConfig) -> Result<(), String> {
    let manifest = read_manifest("manifest", &config.manifest_path, &config.error_report)?;
    let executor = FsTreeMaterializeExecutor {
        entries: manifest.entries().to_vec(),
        manifest,
        sources: config.sources,
        input_roots: config.inputs,
        output_object_dir: config.output_object_dir,
        error_report: config.error_report,
        success_report: config.success_report,
    };
    run_executor(&executor)
}

fn read_manifest(
    label: &str,
    manifest_path: &Path,
    error_report: &Path,
) -> Result<FsTreeManifest, String> {
    let bytes = fs::read(manifest_path).map_err(|error| {
        let report = ExecutorErrorReport {
            kind: "manifest".to_string(),
            path: manifest_path.display().to_string(),
            message: format!(
                "failed to read {label} '{}': {error}",
                manifest_path.display()
            ),
            errno: error.raw_os_error(),
        };
        let _ = write_executor_error_report(error_report, &report);
        report.to_string()
    })?;
    FsTreeManifest::parse_canonical_bytes(&bytes).map_err(|error| {
        let report = ExecutorErrorReport {
            kind: "manifest".to_string(),
            path: manifest_path.display().to_string(),
            message: format!(
                "failed to parse {label} '{}': {error}",
                manifest_path.display()
            ),
            errno: None,
        };
        let _ = write_executor_error_report(error_report, &report);
        report.to_string()
    })
}

fn run_executor(executor: &FsTreeMaterializeExecutor) -> Result<(), String> {
    let existed_before =
        executor.output_object_dir.exists() || executor.output_object_dir.is_symlink();
    match executor.materialize() {
        Ok(report) => {
            write_fs_tree_materialize_report(&executor.success_report, &report).map_err(
                |error| {
                    format!(
                        "failed to write fs-tree materialize success report '{}': {error}",
                        executor.success_report.display()
                    )
                },
            )?;
            Ok(())
        }
        Err(report) => {
            if !existed_before
                && (executor.output_object_dir.exists() || executor.output_object_dir.is_symlink())
            {
                let _ = fs::remove_dir_all(&executor.output_object_dir);
            }
            write_executor_error_report(&executor.error_report, &report).map_err(|error| {
                format!(
                    "failed to write executor error report '{}': {error}; original error: {report}",
                    executor.error_report.display()
                )
            })?;
            Err(report.to_string())
        }
    }
}

#[derive(Debug, Clone)]
struct FsTreeMaterializeExecutor {
    entries: Vec<FsTreeEntry>,
    manifest: FsTreeManifest,
    sources: Vec<FsTreeArchiveEntrySource>,
    input_roots: Vec<PathBuf>,
    output_object_dir: PathBuf,
    error_report: PathBuf,
    success_report: PathBuf,
}

impl FsTreeMaterializeExecutor {
    fn materialize(&self) -> Result<FsTreeMaterializeReport, ExecutorErrorReport> {
        self.validate_shape()?;
        let paths = self.create_object_dir()?;
        let mut report = FsTreeMaterializeReport::default();

        for (entry, source) in self.entries.iter().zip(&self.sources) {
            match (entry, source) {
                (FsTreeEntry::Directory { path, .. }, FsTreeArchiveEntrySource::Directory) => {
                    let start = Instant::now();
                    if !path.is_empty() {
                        fs::create_dir(paths.root_dir.join(path)).map_err(|error| {
                            report_io(
                                "mkdir",
                                &paths.report_root.join(path),
                                format!(
                                    "failed to create fs-tree directory '{}'",
                                    paths.report_root.join(path).display()
                                ),
                                error,
                            )
                        })?;
                    }
                    report.directory_ms += elapsed_ms(start);
                    report.directory_count += 1;
                }
                (
                    FsTreeEntry::File { path, .. },
                    FsTreeArchiveEntrySource::File {
                        input_index,
                        path: source_rel,
                    },
                ) => {
                    let start = Instant::now();
                    let source_path = self.source_path(*input_index, source_rel)?;
                    let dst = paths.root_dir.join(path);
                    fs::hard_link(&source_path, &dst).map_err(|error| {
                        report_io(
                            "hardlink",
                            &paths.report_root.join(path),
                            format!(
                                "failed to hardlink fs-tree file '{}' to '{}'",
                                source_path.display(),
                                paths.report_root.join(path).display()
                            ),
                            error,
                        )
                    })?;
                    validate_file_attrs(&dst, &paths.report_root.join(path), entry)?;
                    report.hardlink_ms += elapsed_ms(start);
                    report.file_count += 1;
                    report.hardlinked_file_count += 1;
                }
                (FsTreeEntry::Symlink { path, target, .. }, FsTreeArchiveEntrySource::Symlink) => {
                    let start = Instant::now();
                    let dst = paths.root_dir.join(path);
                    symlink(target.as_str(), &dst).map_err(|error| {
                        report_io(
                            "symlink",
                            &paths.report_root.join(path),
                            format!(
                                "failed to create fs-tree symlink '{}'",
                                paths.report_root.join(path).display()
                            ),
                            error,
                        )
                    })?;
                    apply_symlink_owner_and_validate(&dst, &paths.report_root.join(path), entry)?;
                    report.symlink_ms += elapsed_ms(start);
                    report.symlink_count += 1;
                }
                _ => {
                    return Err(report_error(
                        "source",
                        &paths.report_root.join(entry.path()),
                        format!(
                            "fs-tree materialize source kind does not match manifest entry '{}'",
                            entry.path()
                        ),
                        None,
                    ));
                }
            }
        }

        let start = Instant::now();
        self.apply_directory_metadata_postorder(&paths)?;
        report.ownership_ms = elapsed_ms(start);
        Ok(report)
    }

    fn validate_shape(&self) -> Result<(), ExecutorErrorReport> {
        if self.entries.len() != self.sources.len() {
            return Err(report_error(
                "source",
                Path::new("/output"),
                format!(
                    "fs-tree materialize source count {} does not match manifest entry count {}",
                    self.sources.len(),
                    self.entries.len()
                ),
                None,
            ));
        }
        Ok(())
    }

    fn create_object_dir(&self) -> Result<MaterializePaths, ExecutorErrorReport> {
        if self.output_object_dir.exists() || self.output_object_dir.is_symlink() {
            return Err(report_error(
                "create",
                &self.output_object_dir,
                format!(
                    "fs-tree output object directory '{}' already exists",
                    self.output_object_dir.display()
                ),
                None,
            ));
        }
        fs::create_dir(&self.output_object_dir).map_err(|error| {
            report_io(
                "create",
                &self.output_object_dir,
                format!(
                    "failed to create fs-tree output object directory '{}'",
                    self.output_object_dir.display()
                ),
                error,
            )
        })?;
        let manifest_path = self.output_object_dir.join(MANIFEST_FILE_NAME);
        self.manifest
            .write_canonical(&manifest_path)
            .map_err(|error| {
                report_error(
                    "manifest",
                    &manifest_path,
                    format!(
                        "failed to write fs-tree manifest '{}': {error}",
                        manifest_path.display()
                    ),
                    None,
                )
            })?;
        let root_dir = self.output_object_dir.join(ROOT_DIR_NAME);
        fs::create_dir(&root_dir).map_err(|error| {
            report_io(
                "mkdir",
                &root_dir,
                format!(
                    "failed to create fs-tree root directory '{}'",
                    root_dir.display()
                ),
                error,
            )
        })?;
        Ok(MaterializePaths {
            root_dir,
            report_root: PathBuf::from("/output/root"),
        })
    }

    fn source_path(
        &self,
        input_index: usize,
        source_rel: &str,
    ) -> Result<PathBuf, ExecutorErrorReport> {
        if source_rel.is_empty() {
            return Err(report_error(
                "path",
                Path::new(source_rel),
                "fs-tree file source path must be relative and non-empty".to_string(),
                None,
            ));
        }
        validate_relative_source_path(source_rel)?;
        let input_root = self.input_roots.get(input_index).ok_or_else(|| {
            report_error(
                "source",
                Path::new(source_rel),
                format!(
                    "fs-tree materialize source references input index {}, but only {} input(s) exist",
                    input_index,
                    self.input_roots.len()
                ),
                None,
            )
        })?;
        Ok(input_root.join(source_rel))
    }

    fn apply_directory_metadata_postorder(
        &self,
        paths: &MaterializePaths,
    ) -> Result<(), ExecutorErrorReport> {
        let tree = build_directory_tree(&self.entries, &paths.root_dir, &paths.report_root)?;
        apply_directory_postorder(&tree, tree.root_index)
    }
}

#[derive(Debug, Clone)]
struct MaterializePaths {
    root_dir: PathBuf,
    report_root: PathBuf,
}

#[derive(Debug)]
struct DirectoryTree {
    entries: Vec<ResolvedDirectory>,
    children: Vec<Vec<usize>>,
    root_index: usize,
}

#[derive(Debug)]
struct ResolvedDirectory {
    manifest_path: String,
    path: PathBuf,
    report_path: PathBuf,
    uid: u32,
    gid: u32,
    mode: u32,
}

fn build_directory_tree(
    entries: &[FsTreeEntry],
    root_dir: &Path,
    report_root: &Path,
) -> Result<DirectoryTree, ExecutorErrorReport> {
    let mut directories = Vec::new();
    for entry in entries {
        if let FsTreeEntry::Directory {
            path,
            uid,
            gid,
            mode,
        } = entry
        {
            let path_on_disk = entry_path(root_dir, path)?;
            directories.push(ResolvedDirectory {
                manifest_path: path.clone(),
                path: path_on_disk,
                report_path: report_path(report_root, path),
                uid: *uid,
                gid: *gid,
                mode: *mode,
            });
        }
    }

    let mut by_path = HashMap::with_capacity(directories.len());
    for (index, entry) in directories.iter().enumerate() {
        by_path.insert(entry.manifest_path.clone(), index);
    }
    let root_index = *by_path.get("").ok_or_else(|| {
        report_error(
            "manifest",
            report_root,
            "fs-tree manifest must contain the root directory".to_string(),
            None,
        )
    })?;

    let mut children = vec![Vec::new(); directories.len()];
    for (index, entry) in directories.iter().enumerate() {
        if index == root_index {
            continue;
        }
        let parent_path = manifest_parent_path(&entry.manifest_path);
        let parent_index = *by_path.get(parent_path).ok_or_else(|| {
            report_error(
                "manifest",
                &entry.report_path,
                format!(
                    "missing parent directory '{}' for fs-tree path '{}'",
                    parent_path, entry.manifest_path
                ),
                None,
            )
        })?;
        children[parent_index].push(index);
    }

    Ok(DirectoryTree {
        entries: directories,
        children,
        root_index,
    })
}

fn apply_directory_postorder(
    tree: &DirectoryTree,
    index: usize,
) -> Result<(), ExecutorErrorReport> {
    for child_index in &tree.children[index] {
        apply_directory_postorder(tree, *child_index)?;
    }

    let entry = &tree.entries[index];
    validate_kind(&entry.path, &entry.report_path, EntryKind::Directory)?;
    chown_if_needed(&entry.path, &entry.report_path, entry.uid, entry.gid)?;
    validate_kind(&entry.path, &entry.report_path, EntryKind::Directory)?;
    chmod(&entry.path, &entry.report_path, entry.mode)?;
    validate_owner_mode(
        &entry.path,
        &entry.report_path,
        EntryKind::Directory,
        entry.uid,
        entry.gid,
        Some(entry.mode),
    )
}

fn validate_file_attrs(
    path: &Path,
    report_path: &Path,
    entry: &FsTreeEntry,
) -> Result<(), ExecutorErrorReport> {
    let FsTreeEntry::File { uid, gid, mode, .. } = entry else {
        unreachable!("caller matched file entry")
    };
    validate_owner_mode(path, report_path, EntryKind::File, *uid, *gid, Some(*mode))
}

fn apply_symlink_owner_and_validate(
    path: &Path,
    report_path: &Path,
    entry: &FsTreeEntry,
) -> Result<(), ExecutorErrorReport> {
    let FsTreeEntry::Symlink { uid, gid, .. } = entry else {
        unreachable!("caller matched symlink entry")
    };
    validate_kind(path, report_path, EntryKind::Symlink)?;
    lchown_if_needed(path, report_path, *uid, *gid)?;
    validate_owner_mode(path, report_path, EntryKind::Symlink, *uid, *gid, None)
}

fn validate_owner_mode(
    path: &Path,
    report_path: &Path,
    expected_kind: EntryKind,
    expected_uid: u32,
    expected_gid: u32,
    expected_mode: Option<u32>,
) -> Result<(), ExecutorErrorReport> {
    let metadata = stat_path(path, report_path)?;
    let actual_kind = EntryKind::from_metadata(&metadata);
    if actual_kind != Some(expected_kind) {
        return Err(report_error(
            "kind",
            report_path,
            format!(
                "fs-tree entry '{}' has kind {}, expected {}",
                report_path.display(),
                actual_kind.map_or("other", EntryKind::as_str),
                expected_kind.as_str()
            ),
            None,
        ));
    }
    if metadata.uid() != expected_uid || metadata.gid() != expected_gid {
        return Err(report_error(
            "owner",
            report_path,
            format!(
                "fs-tree entry '{}' has owner {}:{}, expected {}:{}",
                report_path.display(),
                metadata.uid(),
                metadata.gid(),
                expected_uid,
                expected_gid
            ),
            None,
        ));
    }
    if let Some(expected_mode) = expected_mode {
        let actual_mode = metadata.permissions().mode() & 0o7777;
        if actual_mode != expected_mode {
            return Err(report_error(
                "mode",
                report_path,
                format!(
                    "fs-tree entry '{}' has mode {:o}, expected {:o}",
                    report_path.display(),
                    actual_mode,
                    expected_mode
                ),
                None,
            ));
        }
    }
    Ok(())
}

fn validate_kind(
    path: &Path,
    report_path: &Path,
    expected: EntryKind,
) -> Result<(), ExecutorErrorReport> {
    let metadata = stat_path(path, report_path)?;
    let actual = EntryKind::from_metadata(&metadata);
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(report_error(
            "kind",
            report_path,
            format!(
                "fs-tree entry '{}' has kind {}, expected {}",
                report_path.display(),
                actual.map_or("other", EntryKind::as_str),
                expected.as_str()
            ),
            None,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
}

impl EntryKind {
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

fn entry_path(root: &Path, manifest_path: &str) -> Result<PathBuf, ExecutorErrorReport> {
    validate_relative_source_path(manifest_path)?;
    if manifest_path.is_empty() {
        Ok(root.to_path_buf())
    } else {
        Ok(root.join(manifest_path))
    }
}

fn report_path(root: &Path, manifest_path: &str) -> PathBuf {
    if manifest_path.is_empty() {
        root.to_path_buf()
    } else {
        root.join(manifest_path)
    }
}

fn validate_relative_source_path(path: &str) -> Result<(), ExecutorErrorReport> {
    if Path::new(path).is_absolute() {
        return Err(report_error(
            "path",
            Path::new(path),
            format!("fs-tree source path '{path}' must be relative"),
            None,
        ));
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(report_error(
                    "path",
                    Path::new(path),
                    format!("fs-tree source path '{path}' contains unsafe component"),
                    None,
                ));
            }
        }
    }
    Ok(())
}

fn manifest_parent_path(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

fn stat_path(path: &Path, report_path: &Path) -> Result<fs::Metadata, ExecutorErrorReport> {
    fs::symlink_metadata(path).map_err(|error| {
        report_io(
            if error.kind() == io::ErrorKind::NotFound {
                "missing"
            } else {
                "stat"
            },
            report_path,
            format!(
                "failed to inspect fs-tree entry '{}'",
                report_path.display()
            ),
            error,
        )
    })
}

fn chown_if_needed(
    path: &Path,
    report_path: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), ExecutorErrorReport> {
    let metadata = stat_path(path, report_path)?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }

    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(|error| {
        report_error(
            "chown",
            report_path,
            format!("failed to chown '{}': {error}", report_path.display()),
            Some(error as i32),
        )
    })
}

fn lchown_if_needed(
    path: &Path,
    report_path: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), ExecutorErrorReport> {
    let metadata = stat_path(path, report_path)?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|error| {
        report_error(
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

fn report_error(
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
    report_error(
        kind,
        path,
        format!("{message}: {error}"),
        error.raw_os_error(),
    )
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::runtime_helper_protocol::read_executor_error_report;
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    #[test]
    fn materialize_hardlinks_files_and_creates_symlinks_from_manifest_target() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output.obj");
        let error_report = temp.path().join("error.json");
        let success_report = temp.path().join("success.json");
        fs::create_dir_all(input.join("bin")).unwrap();
        fs::write(input.join("bin/tool"), b"tool\n").unwrap();
        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::directory("bin", owner.0, owner.1, 0o755),
            FsTreeEntry::file("bin/tool", owner.0, owner.1, 0o644),
            FsTreeEntry::symlink("bin/tool-link", owner.0, owner.1, "tool"),
        ])
        .unwrap();
        fs::set_permissions(input.join("bin/tool"), fs::Permissions::from_mode(0o644)).unwrap();
        let executor = FsTreeMaterializeExecutor {
            entries: manifest.entries().to_vec(),
            manifest,
            sources: vec![
                FsTreeArchiveEntrySource::Directory,
                FsTreeArchiveEntrySource::Directory,
                FsTreeArchiveEntrySource::File {
                    input_index: 0,
                    path: "bin/tool".to_string(),
                },
                FsTreeArchiveEntrySource::Symlink,
            ],
            input_roots: vec![input.clone()],
            output_object_dir: output.clone(),
            error_report,
            success_report,
        };

        let report = executor.materialize().unwrap();

        assert_eq!(report.file_count, 1);
        assert_eq!(report.hardlinked_file_count, 1);
        assert_eq!(
            fs::read_to_string(output.join("root/bin/tool")).unwrap(),
            "tool\n"
        );
        assert_eq!(
            fs::read_link(output.join("root/bin/tool-link")).unwrap(),
            PathBuf::from("tool")
        );
        let src = fs::metadata(input.join("bin/tool")).unwrap();
        let dst = fs::metadata(output.join("root/bin/tool")).unwrap();
        assert_eq!((src.dev(), src.ino()), (dst.dev(), dst.ino()));
    }

    #[test]
    fn materialize_reports_file_attr_mismatch_after_hardlink() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output.obj");
        let error_report = temp.path().join("error.json");
        let success_report = temp.path().join("success.json");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("tool"), b"tool\n").unwrap();
        fs::set_permissions(input.join("tool"), fs::Permissions::from_mode(0o644)).unwrap();
        let owner = current_owner();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", owner.0, owner.1, 0o755),
            FsTreeEntry::file("tool", owner.0, owner.1, 0o755),
        ])
        .unwrap();
        let executor = FsTreeMaterializeExecutor {
            entries: manifest.entries().to_vec(),
            manifest,
            sources: vec![
                FsTreeArchiveEntrySource::Directory,
                FsTreeArchiveEntrySource::File {
                    input_index: 0,
                    path: "tool".to_string(),
                },
            ],
            input_roots: vec![input.clone()],
            output_object_dir: output.clone(),
            error_report: error_report.clone(),
            success_report,
        };

        let error = run_executor(&executor).unwrap_err();

        assert!(error.contains("has mode 644, expected 755"));
        assert!(!output.exists());
        assert!(read_executor_error_report(&error_report).unwrap().is_some());
    }

    fn current_owner() -> (u32, u32) {
        (unsafe { libc::geteuid() }, unsafe { libc::getegid() })
    }
}
