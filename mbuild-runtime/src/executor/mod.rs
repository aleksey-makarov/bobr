//! Child-side executors used by runtime containers.

use crate::error::RuntimeError;
pub(crate) use mbuild_core::runtime_helper_protocol::ExecutorErrorReport;
use std::fs;
use std::path::Path;

#[cfg(test)]
pub(crate) fn write_executor_error_report(
    path: &Path,
    report: &ExecutorErrorReport,
) -> Result<(), RuntimeError> {
    mbuild_core::runtime_helper_protocol::write_executor_error_report(path, report)
        .map_err(|error| RuntimeError::Executor(error.to_string()))
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
}
