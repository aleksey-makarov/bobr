//! Helper-side implementation of the `fs-tree-initramfs` operation.

use mbuild_core::runtime_helper_protocol::{
    ExecutorErrorReport, FsTreeArchiveEntrySource, FsTreeInitramfsHelperConfig,
    write_executor_error_report,
};
use mbuild_core::{FsTreeEntry, FsTreeManifest, InitramfsEntrySource, write_newc_initramfs};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Run the fs-tree initramfs operation from a JSON config file path.
pub(crate) fn run_config_path(path: &Path) -> Result<(), String> {
    let config = read_config(path)?;
    run_config(config)
}

fn read_config(path: &Path) -> Result<FsTreeInitramfsHelperConfig, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read helper config '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to parse helper config '{}': {error}",
            path.display()
        )
    })
}

fn run_config(config: FsTreeInitramfsHelperConfig) -> Result<(), String> {
    let manifest = parse_manifest("manifest", &config.manifest, &config.error_report)?;
    let executor = FsTreeInitramfsExecutor {
        entries: manifest.entries().to_vec(),
        sources: config.sources,
        input_roots: config.inputs,
        output: config.output_initramfs,
        error_report: config.error_report,
    };
    run_executor(&executor)
}

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

fn run_executor(executor: &FsTreeInitramfsExecutor) -> Result<(), String> {
    match executor.write_initramfs() {
        Ok(()) => Ok(()),
        Err(report) => {
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
struct FsTreeInitramfsExecutor {
    entries: Vec<FsTreeEntry>,
    sources: Vec<FsTreeArchiveEntrySource>,
    input_roots: Vec<PathBuf>,
    output: PathBuf,
    error_report: PathBuf,
}

impl FsTreeInitramfsExecutor {
    fn write_initramfs(&self) -> Result<(), ExecutorErrorReport> {
        let file = fs::File::create(&self.output).map_err(|error| {
            report_io(
                "create",
                &self.output,
                format!(
                    "failed to create initramfs output '{}'",
                    self.output.display()
                ),
                error,
            )
        })?;
        let sources = initramfs_sources(&self.sources, &self.input_roots)?;
        write_newc_initramfs(file, &self.entries, &sources).map_err(|error| {
            report(
                "write",
                &self.output,
                format!("failed to write initramfs: {error}"),
                None,
            )
        })
    }
}

fn initramfs_sources(
    sources: &[FsTreeArchiveEntrySource],
    input_roots: &[PathBuf],
) -> Result<Vec<InitramfsEntrySource>, ExecutorErrorReport> {
    sources
        .iter()
        .map(|source| match source {
            FsTreeArchiveEntrySource::Directory => Ok(InitramfsEntrySource::Directory),
            FsTreeArchiveEntrySource::File { input_index, path } => {
                let input_root = input_roots.get(*input_index).ok_or_else(|| {
                    report(
                        "source",
                        Path::new(path),
                        format!(
                            "fs-tree initramfs source references input index {}, but only {} input(s) exist",
                            input_index,
                            input_roots.len()
                        ),
                        None,
                    )
                })?;
                Ok(InitramfsEntrySource::File {
                    path: input_root.join(path),
                })
            }
            FsTreeArchiveEntrySource::Symlink => Ok(InitramfsEntrySource::Symlink),
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn initramfs_sources_resolve_file_paths_under_input_roots() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("root");
        let sources = initramfs_sources(
            &[
                FsTreeArchiveEntrySource::Directory,
                FsTreeArchiveEntrySource::File {
                    input_index: 0,
                    path: "bin/init".to_string(),
                },
                FsTreeArchiveEntrySource::Symlink,
            ],
            std::slice::from_ref(&input),
        )
        .unwrap();

        assert_eq!(
            sources,
            vec![
                InitramfsEntrySource::Directory,
                InitramfsEntrySource::File {
                    path: input.join("bin/init"),
                },
                InitramfsEntrySource::Symlink,
            ]
        );
    }
}
