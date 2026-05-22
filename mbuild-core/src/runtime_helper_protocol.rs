//! Wire protocol shared by runtime parent code and runtime helper binaries.
//!
//! This module contains only serialized request/response shapes and small file
//! helpers for helper-owned reports. It intentionally does not contain process
//! lifecycle, namespace setup, or ownership execution code: those belong to
//! `mbuild-runtime` and `mbuild-runtime-helper`.
//!
//! Path fields in this protocol are host/helper namespace paths. The parent
//! must pass paths that are valid for the helper process after namespace setup;
//! for local ownership that means absolute paths in the host filesystem.

use fsobj_hash::ObjectHash;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Runtime helper binary name expected by the parent runtime.
pub const HELPER_BINARY_NAME: &str = "mbuild-runtime-helper";

/// Version of the helper command-line and JSON report protocol.
pub const HELPER_PROTOCOL_VERSION: u32 = 1;

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

/// Optional hash mode requested after ownership materialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnershipHelperHashReport {
    /// Hash `target_root` directly after ownership and mode materialization.
    TargetRoot,
    /// Hash a synthetic fs-tree object from a manifest, root, and extra files.
    FsTreeObject {
        /// Canonical `manifest.jsonl` bytes encoded as UTF-8 text.
        manifest: String,
        /// Additional top-level object files as `(name, content)` byte arrays.
        extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    },
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

    /// Optional absolute path where the helper writes a structured success
    /// result.
    ///
    /// This is optional because not every helper operation has a success
    /// payload. Ownership-only materialization needs only process success or an
    /// `error_report`; hash-producing ownership operations pass `Some(path)` so
    /// the helper can return the computed object hash and timings. If this is
    /// `None`, a successful helper run does not write a result report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_report: Option<PathBuf>,

    /// Canonical materialization manifest bytes encoded as UTF-8 text.
    pub manifest: String,

    /// Optional hash mode requested by the parent.
    ///
    /// When this is `Some`, callers that need the hash result should also set
    /// [`Self::result_report`] to `Some`; otherwise the helper can compute the
    /// hash but has no protocol path to return it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_report: Option<OwnershipHelperHashReport>,

    /// Logical-to-host id mapping configured by the parent.
    pub idmap: OwnershipHelperIdmap,
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

/// Structured helper success report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorResultReport {
    /// Computed object hash encoded as lowercase hex.
    pub object_hash: String,
    /// Optional helper-side phase timings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<ExecutorResultTimings>,
}

/// Helper-side phase timings reported by ownership operations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorResultTimings {
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
    /// Time spent validating materialized entries after mutation.
    pub validate_applied_ms: u128,
    /// Time spent serializing the fs-tree manifest for hashing.
    pub manifest_serialize_ms: u128,
    /// Time spent hashing the target tree or fs-tree object.
    pub hash_ms: u128,
}

/// Decoded helper success report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorResult {
    /// Computed object hash.
    pub object_hash: ObjectHash,
    /// Optional helper-side phase timings.
    pub timings: Option<ExecutorResultTimings>,
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

/// Write a structured helper success report to `path`.
pub fn write_executor_result_report_with_timings(
    path: &Path,
    object_hash: ObjectHash,
    timings: Option<ExecutorResultTimings>,
) -> io::Result<()> {
    let report = ExecutorResultReport {
        object_hash: object_hash.to_string(),
        timings,
    };
    let bytes = serde_json::to_vec(&report).map_err(io::Error::other)?;
    fs::write(path, bytes)
}

/// Read a structured helper success report from `path`.
///
/// An empty file means "no report was written" and returns `Ok(None)`.
pub fn read_executor_result_report(path: &Path) -> io::Result<Option<ExecutorResult>> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }

    let report = serde_json::from_slice::<ExecutorResultReport>(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let object_hash = ObjectHash::from_str(&report.object_hash)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some(ExecutorResult {
        object_hash,
        timings: report.timings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsobj_hash::hash_file_bytes;
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
    fn executor_result_report_round_trips_object_hash_and_timings() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("result.json");
        let object_hash = hash_file_bytes(false, b"test");
        let timings = ExecutorResultTimings {
            total_ms: 10,
            validate_entries_ms: 1,
            chown_ms: 2,
            lchown_ms: 3,
            chmod_files_ms: 4,
            chmod_dirs_ms: 5,
            validate_applied_ms: 6,
            manifest_serialize_ms: 7,
            hash_ms: 8,
        };

        write_executor_result_report_with_timings(&path, object_hash, Some(timings.clone()))
            .unwrap();

        assert_eq!(
            read_executor_result_report(&path).unwrap(),
            Some(ExecutorResult {
                object_hash,
                timings: Some(timings),
            })
        );
    }

    #[test]
    fn ownership_helper_config_serializes_idmap_and_hash_mode() {
        let config = OwnershipHelperConfig {
            target_root: PathBuf::from("/tmp/root"),
            error_report: PathBuf::from("/tmp/error.json"),
            result_report: Some(PathBuf::from("/tmp/result.json")),
            manifest: "{}\n".to_string(),
            hash_report: Some(OwnershipHelperHashReport::FsTreeObject {
                manifest: "{}\n".to_string(),
                extra_files: vec![(b"name".to_vec(), b"value".to_vec())],
            }),
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
        assert_eq!(value["hash_report"]["kind"], "fs_tree_object");

        let decoded: OwnershipHelperConfig = serde_json::from_value(value).unwrap();
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
