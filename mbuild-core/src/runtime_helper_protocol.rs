//! Wire protocol shared by runtime parent code and runtime helper binaries.
//!
//! This module contains only serialized request/response shapes and small file
//! helpers for helper-owned reports. It intentionally does not contain process
//! lifecycle, namespace setup, or ownership execution code: those belong to
//! `mbuild-runtime` and `mbuild-runtime-helper`.
//!
//! Path fields in this protocol are host/helper namespace paths. The parent
//! must pass paths that are valid for the helper process after namespace setup;
//! for the current local helper launcher that means absolute paths in the host
//! filesystem.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Runtime helper binary name expected by the parent runtime.
pub const HELPER_BINARY_NAME: &str = "mbuild-runtime-helper";

/// Version of the helper command-line and JSON report protocol.
pub const HELPER_PROTOCOL_VERSION: u32 = 4;

/// Machine-readable protocol metadata printed by `--protocol-info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperProtocolInfo {
    /// Helper binary protocol name.
    pub name: String,
    /// Helper protocol version.
    pub protocol_version: u32,
}

/// Serializable id mapping passed to the ownership helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipHelperIdmap {
    /// Host uid mapped to logical uid 0.
    pub current_uid: u32,
    /// Host gid mapped to logical gid 0.
    pub current_gid: u32,
    /// First host uid mapped to positive logical uids.
    pub subuid_base: u32,
    /// Number of positive logical uids available.
    pub subuid_count: u32,
    /// First host gid mapped to positive logical gids.
    pub subgid_base: u32,
    /// Number of positive logical gids available.
    pub subgid_count: u32,
}

/// JSON configuration consumed by the ownership helper operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipHelperConfig {
    /// Absolute path to the target root in the helper-visible filesystem.
    ///
    /// The helper applies ownership and mode changes to this tree. The parent
    /// should canonicalize this path before serializing the config so the
    /// helper does not depend on the parent's current working directory.
    pub target_root: PathBuf,

    /// Absolute path where the helper writes a structured failure report.
    ///
    /// The parent creates/truncates this file before launching the helper and
    /// reads it after the helper exits. If the helper fails after it has enough
    /// context to produce a structured error, this file contains an
    /// [`ExecutorErrorReport`].
    pub error_report: PathBuf,

    /// Absolute helper-visible path to a canonical fs-tree manifest file.
    pub manifest_path: PathBuf,

    /// Logical-to-host id mapping configured by the parent.
    pub idmap: OwnershipHelperIdmap,
}

/// Source selected for one manifest entry in an fs-tree archive operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FsTreeArchiveEntrySource {
    /// Directory entry; metadata comes from the manifest.
    Directory,
    /// Regular file entry whose bytes are read from one input root.
    File {
        /// Index into the operation's `inputs` array.
        input_index: usize,
        /// Path relative to the selected input root.
        path: String,
    },
    /// Symlink entry; target and metadata come from the manifest.
    Symlink,
}

/// JSON configuration consumed by the fs-tree tar helper operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsTreeTarHelperConfig {
    /// Absolute path to the output tar file in the helper-visible filesystem.
    pub output_tar: PathBuf,

    /// Absolute path where the helper writes a structured failure report.
    pub error_report: PathBuf,

    /// Absolute helper-visible path to a canonical fs-tree manifest file.
    pub manifest_path: PathBuf,

    /// Absolute helper-visible input roots used by file sources.
    pub inputs: Vec<PathBuf>,

    /// Per-entry source mapping in the same order as `manifest.entries()`.
    pub sources: Vec<FsTreeArchiveEntrySource>,
}

/// JSON configuration consumed by the fs-tree initramfs helper operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsTreeInitramfsHelperConfig {
    /// Absolute path to the output initramfs file in the helper-visible filesystem.
    pub output_initramfs: PathBuf,

    /// Absolute path where the helper writes a structured failure report.
    pub error_report: PathBuf,

    /// Absolute helper-visible path to a canonical fs-tree manifest file.
    pub manifest_path: PathBuf,

    /// Absolute helper-visible input roots used by file sources.
    pub inputs: Vec<PathBuf>,

    /// Per-entry source mapping in the same order as `manifest.entries()`.
    pub sources: Vec<FsTreeArchiveEntrySource>,
}

/// JSON configuration consumed by the fs-tree materialization helper operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsTreeMaterializeHelperConfig {
    /// Absolute path to the output fs-tree object directory in the helper-visible filesystem.
    pub output_object_dir: PathBuf,

    /// Absolute path where the helper writes a structured failure report.
    pub error_report: PathBuf,

    /// Absolute path where the helper writes a structured success report.
    pub success_report: PathBuf,

    /// Absolute helper-visible path to a canonical fs-tree manifest file.
    pub manifest_path: PathBuf,

    /// Absolute helper-visible input roots used by file sources.
    pub inputs: Vec<PathBuf>,

    /// Per-entry source mapping in the same order as `manifest.entries()`.
    pub sources: Vec<FsTreeArchiveEntrySource>,
}

/// Structured success report written by the fs-tree materialization helper.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsTreeMaterializeReport {
    /// Number of directory entries materialized, including the root directory.
    pub directory_count: usize,
    /// Number of regular file entries materialized.
    pub file_count: usize,
    /// Number of regular file entries materialized by hardlink.
    pub hardlinked_file_count: usize,
    /// Number of symlink entries materialized.
    pub symlink_count: usize,
    /// Milliseconds spent creating directories.
    pub directory_ms: u128,
    /// Milliseconds spent hardlinking and validating files.
    pub hardlink_ms: u128,
    /// Milliseconds spent creating and validating symlinks.
    pub symlink_ms: u128,
    /// Milliseconds spent applying ownership and mode metadata.
    pub ownership_ms: u128,
}

/// Structured helper failure report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorErrorReport {
    /// Machine-readable error category such as `missing`, `kind`, or `chown`.
    pub kind: String,
    /// Display path for the object that failed.
    pub path: String,
    /// Human-readable failure message.
    pub message: String,
    /// Optional OS errno when the failure came from an OS operation.
    pub errno: Option<i32>,
}

impl fmt::Display for ExecutorErrorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} error at {}: {}",
            self.kind, self.path, self.message
        )?;
        if let Some(errno) = self.errno {
            write!(formatter, " (errno {errno})")?;
        }
        Ok(())
    }
}

/// Write a structured helper failure report to `path`.
pub fn write_executor_error_report(path: &Path, report: &ExecutorErrorReport) -> io::Result<()> {
    let bytes = serde_json::to_vec(report).map_err(io::Error::other)?;
    fs::write(path, bytes)
}

/// Read a structured helper failure report from `path`.
///
/// An empty file means "no report was written" and returns `Ok(None)`.
pub fn read_executor_error_report(path: &Path) -> io::Result<Option<ExecutorErrorReport>> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Write a structured fs-tree materialization success report to `path`.
pub fn write_fs_tree_materialize_report(
    path: &Path,
    report: &FsTreeMaterializeReport,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(report).map_err(io::Error::other)?;
    fs::write(path, bytes)
}

/// Read a structured fs-tree materialization success report from `path`.
pub fn read_fs_tree_materialize_report(path: &Path) -> io::Result<FsTreeMaterializeReport> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn executor_error_report_serializes_expected_json_shape() {
        let report = test_report();
        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["kind"], "chown");
        assert_eq!(value["path"], "/target/etc/passwd");
        assert_eq!(value["message"], "failed to chown");
        assert_eq!(value["errno"], 1);

        let decoded: ExecutorErrorReport = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, report);
    }

    #[test]
    fn executor_error_report_round_trips_through_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("error.json");
        let report = test_report();

        write_executor_error_report(&path, &report).unwrap();

        assert_eq!(read_executor_error_report(&path).unwrap(), Some(report));
    }

    #[test]
    fn executor_error_report_treats_empty_file_as_no_report() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("error.json");
        fs::write(&path, b"").unwrap();

        assert_eq!(read_executor_error_report(&path).unwrap(), None);
    }

    #[test]
    fn ownership_helper_config_serializes_idmap() {
        let config = OwnershipHelperConfig {
            target_root: PathBuf::from("/tmp/root"),
            error_report: PathBuf::from("/tmp/error.json"),
            manifest_path: PathBuf::from("/tmp/manifest.jsonl"),
            idmap: OwnershipHelperIdmap {
                current_uid: 1000,
                current_gid: 1000,
                subuid_base: 100000,
                subuid_count: 65536,
                subgid_base: 200000,
                subgid_count: 65536,
            },
        };

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value["target_root"], "/tmp/root");
        assert_eq!(value["idmap"]["subuid_base"], 100000);

        let decoded: OwnershipHelperConfig = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn fs_tree_tar_helper_config_serializes_sources() {
        let config = FsTreeTarHelperConfig {
            output_tar: PathBuf::from("/tmp/rootfs.tar"),
            error_report: PathBuf::from("/tmp/error.json"),
            manifest_path: PathBuf::from("/tmp/manifest.jsonl"),
            inputs: vec![PathBuf::from("/tmp/input/root")],
            sources: vec![
                FsTreeArchiveEntrySource::Directory,
                FsTreeArchiveEntrySource::File {
                    input_index: 0,
                    path: "bin/tool".to_string(),
                },
                FsTreeArchiveEntrySource::Symlink,
            ],
        };

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value["output_tar"], "/tmp/rootfs.tar");
        assert_eq!(value["manifest_path"], "/tmp/manifest.jsonl");
        assert_eq!(value["sources"][1]["kind"], "file");
        assert_eq!(value["sources"][1]["path"], "bin/tool");

        let decoded: FsTreeTarHelperConfig = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, config);
    }

    fn test_report() -> ExecutorErrorReport {
        ExecutorErrorReport {
            kind: "chown".to_string(),
            path: "/target/etc/passwd".to_string(),
            message: "failed to chown".to_string(),
            errno: Some(1),
        }
    }
}
