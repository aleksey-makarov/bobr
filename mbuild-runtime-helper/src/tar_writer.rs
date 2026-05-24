//! Helper-side implementation of the `fs-tree-tar` operation.

use mbuild_core::runtime_helper_protocol::{
    ExecutorErrorReport, FsTreeArchiveEntrySource, FsTreeTarHelperConfig,
    write_executor_error_report,
};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Run the fs-tree tar operation from a JSON config file path.
pub(crate) fn run_config_path(path: &Path) -> Result<(), String> {
    let config = read_config(path)?;
    run_config(config)
}

fn read_config(path: &Path) -> Result<FsTreeTarHelperConfig, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read helper config '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to parse helper config '{}': {error}",
            path.display()
        )
    })
}

fn run_config(config: FsTreeTarHelperConfig) -> Result<(), String> {
    let manifest = parse_manifest("manifest", &config.manifest, &config.error_report)?;
    let executor = FsTreeTarExecutor {
        entries: manifest.entries().to_vec(),
        sources: config.sources,
        input_roots: config.inputs,
        output: config.output_tar,
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

fn run_executor(executor: &FsTreeTarExecutor) -> Result<(), String> {
    match executor.write_tar() {
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
struct FsTreeTarExecutor {
    entries: Vec<FsTreeEntry>,
    sources: Vec<FsTreeArchiveEntrySource>,
    input_roots: Vec<PathBuf>,
    output: PathBuf,
    error_report: PathBuf,
}

impl FsTreeTarExecutor {
    fn write_tar(&self) -> Result<(), ExecutorErrorReport> {
        let file = fs::File::create(&self.output).map_err(|error| {
            report_io(
                "create",
                &self.output,
                format!("failed to create tar output '{}'", self.output.display()),
                error,
            )
        })?;
        write_tar_stream(file, &self.entries, &self.sources, &self.input_roots)
    }
}

fn write_tar_stream<W: io::Write>(
    writer: W,
    entries: &[FsTreeEntry],
    sources: &[FsTreeArchiveEntrySource],
    input_roots: &[PathBuf],
) -> Result<(), ExecutorErrorReport> {
    let mut tar = tar::Builder::new(io::BufWriter::new(writer));
    tar.mode(tar::HeaderMode::Deterministic);

    for (entry, source) in entries.iter().zip(sources) {
        if entry.path().is_empty() {
            continue;
        }
        match (entry, source) {
            (FsTreeEntry::Directory { .. }, FsTreeArchiveEntrySource::Directory) => {
                append_directory(&mut tar, entry)?
            }
            (FsTreeEntry::File { .. }, FsTreeArchiveEntrySource::File { input_index, path }) => {
                append_file(&mut tar, entry, *input_index, path, input_roots)?
            }
            (FsTreeEntry::Symlink { .. }, FsTreeArchiveEntrySource::Symlink) => {
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
    input_roots: &[PathBuf],
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
    let input_root = input_roots.get(input_index).ok_or_else(|| {
        report(
            "source",
            Path::new(path),
            format!(
                "fs-tree tar source for '{path}' references input index {}, but only {} input(s) exist",
                input_index,
                input_roots.len()
            ),
            None,
        )
    })?;
    let source = input_root.join(source_rel);
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
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::File {
                input_index: 1,
                path: "etc/config".to_string(),
            },
            FsTreeArchiveEntrySource::Symlink,
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::File {
                input_index: 0,
                path: "usr/bin/tool".to_string(),
            },
        ];

        let mut bytes = Vec::new();
        write_tar_stream(
            &mut bytes,
            manifest.entries(),
            &sources,
            &[input0.clone(), input1.clone()],
        )
        .unwrap();

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
}
