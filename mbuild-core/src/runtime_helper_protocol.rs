use fsobj_hash::ObjectHash;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const HELPER_BINARY_NAME: &str = "mbuild-runtime-helper";
pub const HELPER_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperProtocolInfo {
    pub name: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipHelperIdmap {
    pub current_uid: u32,
    pub current_gid: u32,
    pub subuid_base: u32,
    pub subuid_count: u32,
    pub subgid_base: u32,
    pub subgid_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnershipHelperHashReport {
    TargetRoot,
    FsTreeObject {
        manifest: String,
        extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipHelperConfig {
    pub target_root: PathBuf,
    pub error_report: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_report: Option<PathBuf>,
    pub manifest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_report: Option<OwnershipHelperHashReport>,
    pub idmap: OwnershipHelperIdmap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorErrorReport {
    pub kind: String,
    pub path: String,
    pub message: String,
    pub errno: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorResultReport {
    pub object_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<ExecutorResultTimings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorResultTimings {
    pub total_ms: u128,
    pub validate_entries_ms: u128,
    pub chown_ms: u128,
    pub lchown_ms: u128,
    pub chmod_files_ms: u128,
    pub chmod_dirs_ms: u128,
    pub validate_applied_ms: u128,
    pub manifest_serialize_ms: u128,
    pub hash_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorResult {
    pub object_hash: ObjectHash,
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

pub fn write_executor_error_report(path: &Path, report: &ExecutorErrorReport) -> io::Result<()> {
    let bytes = serde_json::to_vec(report).map_err(io::Error::other)?;
    fs::write(path, bytes)
}

pub fn read_executor_error_report(path: &Path) -> io::Result<Option<ExecutorErrorReport>> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

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
