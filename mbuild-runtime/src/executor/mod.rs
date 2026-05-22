//! Child-side executors used by runtime containers.

use crate::error::RuntimeError;
use fsobj_hash::ObjectHash;
use libcontainer::workload::ExecutorError;
#[cfg(test)]
use mbuild_core::runtime_helper_protocol::OwnershipTimings;
pub(crate) use mbuild_core::runtime_helper_protocol::{
    ExecutorErrorReport, ExecutorResult, ExecutorResultReport,
};
use std::fs;
use std::path::Path;
use std::str::FromStr;

pub(crate) fn write_executor_error_report(
    path: &Path,
    report: &ExecutorErrorReport,
) -> Result<(), ExecutorError> {
    mbuild_core::runtime_helper_protocol::write_executor_error_report(path, report)
        .map_err(executor_error)
}

pub(crate) fn read_executor_error_report(
    path: &Path,
) -> Result<Option<ExecutorErrorReport>, RuntimeError> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }

    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        RuntimeError::Executor(format!(
            "failed to parse executor error report '{}': {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
pub(crate) fn write_executor_result_report(
    path: &Path,
    object_hash: ObjectHash,
) -> Result<(), ExecutorError> {
    write_executor_result_report_with_timings(path, object_hash, None)
}

#[cfg(test)]
pub(crate) fn write_executor_result_report_with_timings(
    path: &Path,
    object_hash: ObjectHash,
    timings: Option<OwnershipTimings>,
) -> Result<(), ExecutorError> {
    mbuild_core::runtime_helper_protocol::write_executor_result_report_with_timings(
        path,
        object_hash,
        timings,
    )
    .map_err(executor_error)
}

#[cfg(test)]
pub(crate) fn read_executor_result_report(path: &Path) -> Result<Option<ObjectHash>, RuntimeError> {
    Ok(read_executor_result_report_with_timings(path)?.map(|report| report.object_hash))
}

pub(crate) fn read_executor_result_report_with_timings(
    path: &Path,
) -> Result<Option<ExecutorResult>, RuntimeError> {
    let bytes = fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RuntimeError::Executor(format!(
                "executor result report '{}' is missing",
                path.display()
            ))
        } else {
            RuntimeError::Io(error)
        }
    })?;
    if bytes.is_empty() {
        return Ok(None);
    }

    let report = serde_json::from_slice::<ExecutorResultReport>(&bytes).map_err(|error| {
        RuntimeError::Executor(format!(
            "failed to parse executor result report '{}': {error}",
            path.display()
        ))
    })?;
    let object_hash = ObjectHash::from_str(&report.object_hash).map_err(|error| {
        RuntimeError::Executor(format!(
            "failed to parse executor result hash '{}': {error}",
            path.display()
        ))
    })?;
    Ok(Some(ExecutorResult {
        object_hash,
        timings: report.timings,
    }))
}

fn executor_error(error: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Other(error.to_string())
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
    fn write_executor_error_report_creates_readable_report() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("error.json");
        let report = test_report();

        write_executor_error_report(&path, &report).unwrap();

        assert_eq!(read_executor_error_report(&path).unwrap(), Some(report));
    }

    #[test]
    fn read_executor_error_report_treats_empty_file_as_no_report() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("error.json");
        fs::write(&path, b"").unwrap();

        assert_eq!(read_executor_error_report(&path).unwrap(), None);
    }

    #[test]
    fn read_executor_error_report_rejects_malformed_non_empty_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("error.json");
        fs::write(&path, b"not json").unwrap();

        let error = read_executor_error_report(&path).unwrap_err();

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert!(
            error
                .to_string()
                .contains("failed to parse executor error report")
        );
    }

    #[test]
    fn read_executor_error_report_returns_io_for_missing_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("missing-error.json");

        let error = read_executor_error_report(&path).unwrap_err();

        assert!(matches!(error, RuntimeError::Io(_)));
    }

    #[test]
    fn executor_result_report_serializes_object_hash() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("result.json");
        let object_hash = test_object_hash();

        write_executor_result_report(&path, object_hash).unwrap();

        assert_eq!(
            read_executor_result_report(&path).unwrap(),
            Some(object_hash)
        );
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["object_hash"], object_hash.to_string());
    }

    #[test]
    fn executor_result_report_serializes_optional_timings() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("result.json");
        let object_hash = test_object_hash();
        let timings = OwnershipTimings {
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

        let report = read_executor_result_report_with_timings(&path)
            .unwrap()
            .unwrap();
        assert_eq!(report.object_hash, object_hash);
        assert_eq!(report.timings, Some(timings));
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["timings"]["hash_ms"], 8);
    }

    #[test]
    fn executor_result_report_accepts_missing_timings() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("result.json");
        let object_hash = test_object_hash();
        fs::write(&path, format!(r#"{{"object_hash":"{}"}}"#, object_hash)).unwrap();

        let report = read_executor_result_report_with_timings(&path)
            .unwrap()
            .unwrap();

        assert_eq!(report.object_hash, object_hash);
        assert_eq!(report.timings, None);
    }

    #[test]
    fn read_executor_result_report_treats_empty_file_as_no_report() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("result.json");
        fs::write(&path, b"").unwrap();

        assert_eq!(read_executor_result_report(&path).unwrap(), None);
    }

    #[test]
    fn read_executor_result_report_rejects_malformed_json() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("result.json");
        fs::write(&path, b"not json").unwrap();

        let error = read_executor_result_report(&path).unwrap_err();

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert!(
            error
                .to_string()
                .contains("failed to parse executor result report")
        );
    }

    #[test]
    fn read_executor_result_report_rejects_invalid_hash() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("result.json");
        fs::write(&path, br#"{"object_hash":"not-a-hash"}"#).unwrap();

        let error = read_executor_result_report(&path).unwrap_err();

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert!(
            error
                .to_string()
                .contains("failed to parse executor result hash")
        );
    }

    #[test]
    fn read_executor_result_report_returns_executor_for_missing_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("missing-result.json");

        let error = read_executor_result_report(&path).unwrap_err();

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert!(error.to_string().contains("result report"));
    }

    #[test]
    fn executor_error_report_display_includes_errno_when_present() {
        assert_eq!(
            test_report().to_string(),
            "chown error at /target/etc/passwd: failed to chown (errno 1)"
        );

        let report = ExecutorErrorReport {
            errno: None,
            ..test_report()
        };
        assert_eq!(
            report.to_string(),
            "chown error at /target/etc/passwd: failed to chown"
        );
    }

    fn test_report() -> ExecutorErrorReport {
        ExecutorErrorReport {
            kind: "chown".to_string(),
            path: "/target/etc/passwd".to_string(),
            message: "failed to chown".to_string(),
            errno: Some(1),
        }
    }

    fn test_object_hash() -> ObjectHash {
        ObjectHash::from_str("1111111111111111111111111111111111111111111111111111111111111111")
            .unwrap()
    }
}
