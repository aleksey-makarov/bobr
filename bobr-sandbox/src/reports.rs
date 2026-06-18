use bobr_runtime::runtime::RuntimeError;
use mbuild_sandbox_runner_core::{
    SandboxRunnerFailureReport, SandboxRunnerSuccessReport, SandboxStepReport,
};
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

pub(crate) fn read_sandbox_success_steps(
    path: &Path,
) -> Result<Vec<SandboxStepReport>, RuntimeError> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Err(RuntimeError::new(format!(
            "sandbox success report '{}' is empty",
            path.display()
        )));
    }
    let report = serde_json::from_slice::<SandboxRunnerSuccessReport>(&bytes).map_err(|error| {
        RuntimeError::new(format!(
            "failed to parse sandbox success report '{}': {error}",
            path.display()
        ))
    })?;
    Ok(report.steps)
}

pub(crate) fn read_sandbox_failure_report(path: &Path, fallback: String) -> RuntimeError {
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => RuntimeError::new(fallback),
        Ok(bytes) => match serde_json::from_slice::<SandboxRunnerFailureReport>(&bytes) {
            Ok(report) => RuntimeError::new(report.to_error_message()),
            Err(error) => RuntimeError::new(format!(
                "{fallback}; failed to parse sandbox failure report '{}': {error}",
                path.display()
            )),
        },
        Err(error) => RuntimeError::new(format!(
            "{fallback}; failed to read sandbox failure report '{}': {error}",
            path.display()
        )),
    }
}

pub(crate) fn status_message(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => match status.signal() {
            Some(signal) => format!("signal {signal}"),
            None => "unknown status".to_string(),
        },
    }
}

pub(crate) fn command_context(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| format!(": {line}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn sandbox_failure_report_message_includes_step_status_and_logs() {
        let report = SandboxRunnerFailureReport {
            label: "compile".to_string(),
            message: "sandbox step 'compile' failed with exit status 2".to_string(),
            exit_code: Some(2),
            signal: None,
            duration_ms: Some(123),
            stdout_path: Some(PathBuf::from("/tmp/compile.stdout")),
            stderr_path: Some(PathBuf::from("/tmp/compile.stderr")),
        };

        let message = report.to_error_message();

        assert!(message.contains("compile"));
        assert!(message.contains("exit_status=2"));
        assert!(message.contains("duration_ms=123"));
        assert!(message.contains("stdout=/tmp/compile.stdout"));
        assert!(message.contains("stderr=/tmp/compile.stderr"));
    }

    #[test]
    fn success_report_reads_only_steps_for_v2_output() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("success.json");
        let report = SandboxRunnerSuccessReport {
            object_hash: "00".repeat(32),
            manifest_jsonl: "legacy manifest ignored".to_string(),
            steps: vec![SandboxStepReport {
                name: "install".to_string(),
                run_as: "build-user".to_string(),
                exit_code: 0,
                duration_ms: 7,
                stdout_path: PathBuf::from("/tmp/install.stdout"),
                stderr_path: PathBuf::from("/tmp/install.stderr"),
            }],
        };
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();

        let steps = read_sandbox_success_steps(&path).unwrap();

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "install");
    }

    #[test]
    fn setup_failure_context_includes_child_stderr_line() {
        let context = command_context(b"\nchild setup detail\nsecond line\n");

        assert_eq!(context, ": child setup detail");
    }
}
