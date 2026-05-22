//! Tar writer support for fs-tree consumers.

use crate::{
    bundle::create_bundle,
    error::RuntimeError,
    executor::{ExecutorErrorReport, write_executor_error_report},
    idmap::MbuildIdmap,
    preflight::preflight_ownership_runtime,
    run::run_init_with_executor,
    spec::build_tar_writer_spec,
};
use libcontainer::oci_spec::runtime::Spec;
use libcontainer::workload::{
    Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError,
};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

/// A host fs-tree root that will be bind-mounted read-only for tar generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeTarInput {
    /// Host path to the input object's `root/` directory.
    pub root_dir: PathBuf,
}

/// The physical source selected for one entry in the output tar stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeTarEntrySource {
    /// Directory entry; metadata comes from the manifest.
    Directory,
    /// Regular file entry whose bytes are read from one mounted input root.
    File {
        /// Index into the `FsTreeTarInput` slice.
        input_index: usize,
        /// Path relative to the selected input root.
        path: String,
    },
    /// Symlink entry; target and metadata come from the manifest.
    Symlink,
}

/// Write a deterministic tar stream for an fs-tree manifest in the ownership
/// user namespace.
///
/// `sources` must have the same length and order as `manifest.entries()`.
/// Regular file bytes are read from read-only input-root bind mounts inside the
/// namespace, while `output_tar` is created through a writable bind mount of its
/// parent directory.
pub fn write_fs_tree_tar_in_ownership_namespace(
    inputs: &[FsTreeTarInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeTarEntrySource],
    output_tar: &Path,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    validate_request(inputs, manifest, sources, output_tar, workspace, idmap)?;
    preflight_ownership_runtime(idmap)?;

    let output_dir = output_tar.parent().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "tar output path '{}' has no parent directory",
            output_tar.display()
        ))
    })?;
    let output_name = output_tar.file_name().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "tar output path '{}' has no file name",
            output_tar.display()
        ))
    })?;
    let output_name = output_name.to_str().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "tar output file name for '{}' is not UTF-8",
            output_tar.display()
        ))
    })?;

    let input_roots = inputs
        .iter()
        .map(|input| input.root_dir.clone())
        .collect::<Vec<_>>();
    let spec = build_tar_writer_spec(idmap, &input_roots, output_dir)?;
    let bundle = create_bundle(workspace, &spec)?;
    prepare_mount_targets(bundle.rootfs_dir(), inputs.len())?;

    let executor = FsTreeTarExecutor {
        entries: manifest.entries().to_vec(),
        sources: sources.to_vec(),
        output_inside: PathBuf::from("/out").join(output_name),
        error_log_inside: PathBuf::from("/error.json"),
    };
    run_init_with_executor(&bundle, workspace, executor)?;
    Ok(())
}

fn validate_request(
    inputs: &[FsTreeTarInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeTarEntrySource],
    output_tar: &Path,
    workspace: &Path,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    if inputs.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "fs-tree tar generation requires at least one input root".to_string(),
        ));
    }
    if manifest.entries().len() != sources.len() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree tar generation source count {} does not match manifest entry count {}",
            sources.len(),
            manifest.entries().len()
        )));
    }
    for (index, input) in inputs.iter().enumerate() {
        if !input.root_dir.is_dir() {
            return Err(RuntimeError::InvalidInput(format!(
                "fs-tree tar input {index} root '{}' must exist and be a directory",
                input.root_dir.display()
            )));
        }
    }
    for entry in manifest.entries() {
        let (uid, gid) = entry_owner(entry);
        idmap.physical_uid(uid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry.path()))
        })?;
        idmap.physical_gid(gid).map_err(|error| {
            RuntimeError::InvalidInput(format!("fs-tree entry '{}': {error}", entry.path()))
        })?;
    }
    for (entry, source) in manifest.entries().iter().zip(sources) {
        match (entry, source) {
            (FsTreeEntry::Directory { .. }, FsTreeTarEntrySource::Directory) => {}
            (FsTreeEntry::File { .. }, FsTreeTarEntrySource::File { input_index, path }) => {
                if *input_index >= inputs.len() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "fs-tree tar source for '{}' references input index {}, but only {} input(s) exist",
                        entry.path(),
                        input_index,
                        inputs.len()
                    )));
                }
                validate_relative_path(path)?;
            }
            (FsTreeEntry::Symlink { .. }, FsTreeTarEntrySource::Symlink) => {}
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "fs-tree tar source kind does not match manifest entry '{}'",
                    entry.path()
                )));
            }
        }
    }
    if !workspace.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree tar workspace '{}' must exist and be a directory",
            workspace.display()
        )));
    }
    let output_dir = output_tar.parent().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "tar output path '{}' has no parent directory",
            output_tar.display()
        ))
    })?;
    if !output_dir.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "tar output directory '{}' must exist and be a directory",
            output_dir.display()
        )));
    }
    Ok(())
}

fn prepare_mount_targets(rootfs: &Path, input_count: usize) -> Result<(), RuntimeError> {
    fs::create_dir_all(rootfs.join("inputs"))?;
    for index in 0..input_count {
        fs::create_dir(rootfs.join("inputs").join(index.to_string()))?;
    }
    fs::create_dir(rootfs.join("out"))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct FsTreeTarExecutor {
    entries: Vec<FsTreeEntry>,
    sources: Vec<FsTreeTarEntrySource>,
    output_inside: PathBuf,
    error_log_inside: PathBuf,
}

impl FsTreeTarExecutor {
    fn write_tar(&self) -> Result<(), ExecutorErrorReport> {
        let file = fs::File::create(&self.output_inside).map_err(|error| {
            report_io(
                "create",
                &self.output_inside,
                format!(
                    "failed to create tar output '{}'",
                    self.output_inside.display()
                ),
                error,
            )
        })?;
        write_tar_stream(file, &self.entries, &self.sources, Path::new("/inputs"))
    }
}

impl Executor for FsTreeTarExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        match self.write_tar() {
            Ok(()) => std::process::exit(0),
            Err(report) => {
                write_executor_error_report(&self.error_log_inside, &report)?;
                Err(ExecutorError::Other(report.to_string()))
            }
        }
    }
}

fn write_tar_stream<W: io::Write>(
    writer: W,
    entries: &[FsTreeEntry],
    sources: &[FsTreeTarEntrySource],
    input_mount_root: &Path,
) -> Result<(), ExecutorErrorReport> {
    let mut tar = tar::Builder::new(io::BufWriter::new(writer));
    tar.mode(tar::HeaderMode::Deterministic);

    for (entry, source) in entries.iter().zip(sources) {
        if entry.path().is_empty() {
            continue;
        }
        match (entry, source) {
            (FsTreeEntry::Directory { .. }, FsTreeTarEntrySource::Directory) => {
                append_directory(&mut tar, entry)?
            }
            (FsTreeEntry::File { .. }, FsTreeTarEntrySource::File { input_index, path }) => {
                append_file(&mut tar, entry, *input_index, path, input_mount_root)?
            }
            (FsTreeEntry::Symlink { .. }, FsTreeTarEntrySource::Symlink) => {
                append_symlink(&mut tar, entry)?
            }
            _ => {
                return Err(report(
                    "source",
                    Path::new(entry.path()),
                    format!(
                        "fs-tree tar source kind does not match manifest entry '{}'",
                        entry.path()
                    ),
                    None,
                ));
            }
        }
    }

    let mut writer = tar.into_inner().map_err(|error| {
        report(
            "finalize",
            Path::new("/out"),
            format!("failed to finalize fs-tree tar stream: {error}"),
            None,
        )
    })?;
    writer.flush().map_err(|error| {
        report_io(
            "flush",
            Path::new("/out"),
            "failed to flush fs-tree tar stream".to_string(),
            error,
        )
    })?;
    Ok(())
}

fn append_directory<W: io::Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
) -> Result<(), ExecutorErrorReport> {
    let FsTreeEntry::Directory {
        path,
        uid,
        gid,
        mode,
    } = entry
    else {
        unreachable!("caller matched directory entry")
    };
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(*mode);
    header.set_mtime(0);
    header.set_size(0);
    header.set_cksum();
    tar.append_data(&mut header, format!("{path}/"), io::empty())
        .map_err(|error| {
            report(
                "append",
                Path::new(path),
                format!("failed to append directory '{path}' to fs-tree tar: {error}"),
                None,
            )
        })
}

fn append_file<W: io::Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
    input_index: usize,
    source_rel: &str,
    input_mount_root: &Path,
) -> Result<(), ExecutorErrorReport> {
    let FsTreeEntry::File {
        path,
        uid,
        gid,
        mode,
        ..
    } = entry
    else {
        unreachable!("caller matched file entry")
    };
    let source = input_mount_root
        .join(input_index.to_string())
        .join(source_rel);
    let metadata = fs::metadata(&source).map_err(|error| {
        report_io(
            "stat",
            &source,
            format!(
                "failed to stat fs-tree tar source file '{}'",
                source.display()
            ),
            error,
        )
    })?;
    let mut file = fs::File::open(&source).map_err(|error| {
        report_io(
            "open",
            &source,
            format!(
                "failed to open fs-tree tar source file '{}'",
                source.display()
            ),
            error,
        )
    })?;
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(*mode);
    header.set_mtime(0);
    header.set_size(metadata.len());
    header.set_cksum();
    tar.append_data(&mut header, path, &mut file)
        .map_err(|error| {
            report(
                "append",
                Path::new(path),
                format!("failed to append file '{path}' to fs-tree tar: {error}"),
                None,
            )
        })
}

fn append_symlink<W: io::Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
) -> Result<(), ExecutorErrorReport> {
    let FsTreeEntry::Symlink {
        path,
        uid,
        gid,
        target,
        ..
    } = entry
    else {
        unreachable!("caller matched symlink entry")
    };
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(0o777);
    header.set_mtime(0);
    header.set_size(0);
    header.set_link_name(target).map_err(|error| {
        report(
            "link-name",
            Path::new(path),
            format!("failed to encode symlink target '{target}' for '{path}': {error}"),
            None,
        )
    })?;
    header.set_cksum();
    tar.append_data(&mut header, path, io::empty())
        .map_err(|error| {
            report(
                "append",
                Path::new(path),
                format!("failed to append symlink '{path}' to fs-tree tar: {error}"),
                None,
            )
        })
}

fn entry_owner(entry: &FsTreeEntry) -> (u32, u32) {
    match entry {
        FsTreeEntry::File { uid, gid, .. }
        | FsTreeEntry::Directory { uid, gid, .. }
        | FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid),
    }
}

fn validate_relative_path(path: &str) -> Result<(), RuntimeError> {
    if path.is_empty() || Path::new(path).is_absolute() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree tar source path '{path}' must be relative and non-empty"
        )));
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "fs-tree tar source path '{path}' contains unsafe component"
                )));
            }
        }
    }
    Ok(())
}

fn report(
    label: impl Into<String>,
    path: &Path,
    message: impl Into<String>,
    errno: Option<i32>,
) -> ExecutorErrorReport {
    ExecutorErrorReport {
        kind: label.into(),
        path: path.display().to_string(),
        message: message.into(),
        errno,
    }
}

fn report_io(
    label: impl Into<String>,
    path: &Path,
    message: String,
    error: io::Error,
) -> ExecutorErrorReport {
    report(
        label,
        path,
        format!("{message}: {error}"),
        error.raw_os_error(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn tar_stream_uses_manifest_order_metadata_and_file_sources() {
        let temp = tempdir().unwrap();
        let input0 = temp.path().join("0");
        let input1 = temp.path().join("1");
        fs::create_dir_all(input0.join("usr/bin")).unwrap();
        fs::create_dir_all(input1.join("etc")).unwrap();
        fs::write(input0.join("usr/bin/tool"), b"tool\n").unwrap();
        fs::write(input1.join("etc/config"), b"config\n").unwrap();

        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("etc", 7, 8, 0o750),
            FsTreeEntry::file("etc/config", 7, 8, 0o640),
            FsTreeEntry::symlink("link", 9, 10, "usr/bin/tool"),
            FsTreeEntry::directory("usr", 1, 2, 0o755),
            FsTreeEntry::directory("usr/bin", 1, 2, 0o755),
            FsTreeEntry::file("usr/bin/tool", 3, 4, 0o755),
        ])
        .unwrap();
        let sources = vec![
            FsTreeTarEntrySource::Directory,
            FsTreeTarEntrySource::Directory,
            FsTreeTarEntrySource::File {
                input_index: 1,
                path: "etc/config".to_string(),
            },
            FsTreeTarEntrySource::Symlink,
            FsTreeTarEntrySource::Directory,
            FsTreeTarEntrySource::Directory,
            FsTreeTarEntrySource::File {
                input_index: 0,
                path: "usr/bin/tool".to_string(),
            },
        ];

        let mut bytes = Vec::new();
        write_tar_stream(&mut bytes, manifest.entries(), &sources, temp.path()).unwrap();

        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut contents = Vec::new();
            io::copy(&mut entry, &mut contents).unwrap();
            seen.push((
                path,
                header.entry_type(),
                header.uid().unwrap(),
                header.gid().unwrap(),
                header.mode().unwrap(),
                header.mtime().unwrap(),
                contents,
                header.link_name().unwrap().map(|p| p.into_owned()),
            ));
        }

        assert_eq!(
            seen.iter()
                .map(|entry| entry.0.as_str())
                .collect::<Vec<_>>(),
            vec![
                "etc/",
                "etc/config",
                "link",
                "usr/",
                "usr/bin/",
                "usr/bin/tool"
            ]
        );
        assert_eq!(seen[1].1, tar::EntryType::Regular);
        assert_eq!(
            (seen[1].2, seen[1].3, seen[1].4, seen[1].5),
            (7, 8, 0o640, 0)
        );
        assert_eq!(seen[1].6, b"config\n");
        assert_eq!(seen[2].1, tar::EntryType::Symlink);
        assert_eq!(
            (seen[2].2, seen[2].3, seen[2].4, seen[2].5),
            (9, 10, 0o777, 0)
        );
        assert_eq!(seen[2].7.as_deref(), Some(Path::new("usr/bin/tool")));
        assert_eq!(seen[5].6, b"tool\n");
    }

    #[test]
    fn validate_request_rejects_source_count_mismatch() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        fs::create_dir(&input).unwrap();
        let output = temp.path().join("out.tar");
        let manifest =
            FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap();
        let idmap = MbuildIdmap::for_tests(1000, 1000, 100000, 10, 200000, 10);

        let error = validate_request(
            &[FsTreeTarInput { root_dir: input }],
            &manifest,
            &[],
            &output,
            temp.path(),
            &idmap,
        )
        .unwrap_err();

        assert!(error.to_string().contains("source count"));
    }

    #[test]
    fn prepare_mount_targets_creates_expected_directories() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("proc")).unwrap();
        prepare_mount_targets(temp.path(), 2).unwrap();
        assert!(temp.path().join("inputs/0").is_dir());
        assert!(temp.path().join("inputs/1").is_dir());
        assert!(temp.path().join("out").is_dir());
        fs::set_permissions(temp.path().join("out"), fs::Permissions::from_mode(0o755)).unwrap();
    }
}
