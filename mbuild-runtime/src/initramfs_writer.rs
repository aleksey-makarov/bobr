//! Initramfs writer support for fs-tree consumers.

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
use mbuild_core::{FsTreeEntry, FsTreeManifest, InitramfsEntrySource, write_newc_initramfs};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// A host fs-tree root that will be bind-mounted read-only for initramfs generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeInitramfsInput {
    /// Host path to the input object's `root/` directory.
    pub root_dir: PathBuf,
}

/// The physical source selected for one entry in the output initramfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeInitramfsEntrySource {
    /// Directory entry; metadata comes from the manifest.
    Directory,
    /// Regular file entry whose bytes are read from one mounted input root.
    File {
        /// Index into the `FsTreeInitramfsInput` slice.
        input_index: usize,
        /// Path relative to the selected input root.
        path: String,
    },
    /// Symlink entry; target and metadata come from the manifest.
    Symlink,
}

/// Write a deterministic Linux `newc` initramfs for an fs-tree manifest in the
/// ownership user namespace.
///
/// `sources` must have the same length and order as `manifest.entries()`.
/// Regular file bytes are read from read-only input-root bind mounts inside the
/// namespace, while `output_initramfs` is created through a writable bind mount
/// of its parent directory.
pub fn write_fs_tree_initramfs_in_ownership_namespace(
    inputs: &[FsTreeInitramfsInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeInitramfsEntrySource],
    output_initramfs: &Path,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    validate_request(
        inputs,
        manifest,
        sources,
        output_initramfs,
        workspace,
        idmap,
    )?;
    preflight_ownership_runtime(idmap)?;

    let output_dir = output_initramfs.parent().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "initramfs output path '{}' has no parent directory",
            output_initramfs.display()
        ))
    })?;
    let output_name = output_initramfs.file_name().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "initramfs output path '{}' has no file name",
            output_initramfs.display()
        ))
    })?;
    let output_name = output_name.to_str().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "initramfs output file name for '{}' is not UTF-8",
            output_initramfs.display()
        ))
    })?;

    let input_roots = inputs
        .iter()
        .map(|input| input.root_dir.clone())
        .collect::<Vec<_>>();
    let spec = build_tar_writer_spec(idmap, &input_roots, output_dir)?;
    let bundle = create_bundle(workspace, &spec)?;
    prepare_mount_targets(bundle.rootfs_dir(), inputs.len())?;

    let executor = FsTreeInitramfsExecutor {
        entries: manifest.entries().to_vec(),
        sources: sources.to_vec(),
        output_inside: PathBuf::from("/out").join(output_name),
        error_log_inside: PathBuf::from("/error.json"),
    };
    run_init_with_executor(&bundle, workspace, executor)?;
    Ok(())
}

fn validate_request(
    inputs: &[FsTreeInitramfsInput],
    manifest: &FsTreeManifest,
    sources: &[FsTreeInitramfsEntrySource],
    output_initramfs: &Path,
    workspace: &Path,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    if inputs.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "fs-tree initramfs generation requires at least one input root".to_string(),
        ));
    }
    if manifest.entries().len() != sources.len() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree initramfs generation source count {} does not match manifest entry count {}",
            sources.len(),
            manifest.entries().len()
        )));
    }
    for (index, input) in inputs.iter().enumerate() {
        if !input.root_dir.is_dir() {
            return Err(RuntimeError::InvalidInput(format!(
                "fs-tree initramfs input {index} root '{}' must exist and be a directory",
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
            (FsTreeEntry::Directory { .. }, FsTreeInitramfsEntrySource::Directory) => {}
            (FsTreeEntry::File { .. }, FsTreeInitramfsEntrySource::File { input_index, path }) => {
                if *input_index >= inputs.len() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "fs-tree initramfs source for '{}' references input index {}, but only {} input(s) exist",
                        entry.path(),
                        input_index,
                        inputs.len()
                    )));
                }
                validate_relative_path(path)?;
            }
            (FsTreeEntry::Symlink { .. }, FsTreeInitramfsEntrySource::Symlink) => {}
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "fs-tree initramfs source kind does not match manifest entry '{}'",
                    entry.path()
                )));
            }
        }
    }
    if !workspace.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "fs-tree initramfs workspace '{}' must exist and be a directory",
            workspace.display()
        )));
    }
    let output_dir = output_initramfs.parent().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "initramfs output path '{}' has no parent directory",
            output_initramfs.display()
        ))
    })?;
    if !output_dir.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "initramfs output directory '{}' must exist and be a directory",
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
struct FsTreeInitramfsExecutor {
    entries: Vec<FsTreeEntry>,
    sources: Vec<FsTreeInitramfsEntrySource>,
    output_inside: PathBuf,
    error_log_inside: PathBuf,
}

impl FsTreeInitramfsExecutor {
    fn write_initramfs(&self) -> Result<(), ExecutorErrorReport> {
        let file = fs::File::create(&self.output_inside).map_err(|error| {
            report_io(
                "create",
                &self.output_inside,
                format!(
                    "failed to create initramfs output '{}'",
                    self.output_inside.display()
                ),
                error,
            )
        })?;
        let sources = initramfs_sources_inside(&self.sources, Path::new("/inputs"))?;
        write_newc_initramfs(file, &self.entries, &sources).map_err(|error| {
            report(
                "write",
                &self.output_inside,
                format!("failed to write initramfs: {error}"),
                None,
            )
        })
    }
}

impl Executor for FsTreeInitramfsExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        match self.write_initramfs() {
            Ok(()) => std::process::exit(0),
            Err(report) => {
                write_executor_error_report(&self.error_log_inside, &report)?;
                Err(ExecutorError::Other(report.to_string()))
            }
        }
    }
}

fn initramfs_sources_inside(
    sources: &[FsTreeInitramfsEntrySource],
    input_mount_root: &Path,
) -> Result<Vec<InitramfsEntrySource>, ExecutorErrorReport> {
    sources
        .iter()
        .map(|source| match source {
            FsTreeInitramfsEntrySource::Directory => Ok(InitramfsEntrySource::Directory),
            FsTreeInitramfsEntrySource::File { input_index, path } => {
                Ok(InitramfsEntrySource::File {
                    path: input_mount_root.join(input_index.to_string()).join(path),
                })
            }
            FsTreeInitramfsEntrySource::Symlink => Ok(InitramfsEntrySource::Symlink),
        })
        .collect()
}

fn validate_relative_path(path: &str) -> Result<(), RuntimeError> {
    if path.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "fs-tree initramfs file source path must not be empty".to_string(),
        ));
    }
    let path = Path::new(path);
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "fs-tree initramfs file source path '{}' must be relative and stay within its input root",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn entry_owner(entry: &FsTreeEntry) -> (u32, u32) {
    match entry {
        FsTreeEntry::File { uid, gid, .. }
        | FsTreeEntry::Directory { uid, gid, .. }
        | FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid),
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
