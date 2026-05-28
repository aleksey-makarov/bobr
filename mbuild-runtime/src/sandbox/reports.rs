use crate::error::RuntimeError;
use fsobj_hash::ObjectHash;
use mbuild_core::FsTreeManifest;
pub use mbuild_sandbox_runner_core::SandboxStepReport;
use mbuild_sandbox_runner_core::{SandboxRunnerFailureReport, SandboxRunnerSuccessReport};
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::str::FromStr;

/// Result of a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxBuildOutcome {
    /// Runtime-side output object hash.
    pub object_hash: ObjectHash,
    /// Canonical manifest built from the actual sandbox output tree.
    pub manifest: FsTreeManifest,
    /// Structured per-step reports.
    pub steps: Vec<SandboxStepReport>,
}

pub(super) fn read_sandbox_success_report(
    path: &Path,
) -> Result<SandboxBuildOutcome, RuntimeError> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Err(RuntimeError::Executor(format!(
            "sandbox success report '{}' is empty",
            path.display()
        )));
    }
    let report = serde_json::from_slice::<SandboxRunnerSuccessReport>(&bytes).map_err(|error| {
        RuntimeError::Executor(format!(
            "failed to parse sandbox success report '{}': {error}",
            path.display()
        ))
    })?;
    let object_hash = ObjectHash::from_str(&report.object_hash).map_err(|error| {
        RuntimeError::Executor(format!(
            "failed to parse sandbox output hash '{}': {error}",
            path.display()
        ))
    })?;
    let manifest = FsTreeManifest::parse_canonical_bytes(report.manifest_jsonl.as_bytes())
        .map_err(|error| {
            RuntimeError::Executor(format!(
                "failed to parse sandbox output manifest '{}': {error}",
                path.display()
            ))
        })?;
    Ok(SandboxBuildOutcome {
        object_hash,
        manifest,
        steps: report.steps,
    })
}

pub(super) fn read_sandbox_failure_report(path: &Path, fallback: String) -> RuntimeError {
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => RuntimeError::Executor(fallback),
        Ok(bytes) => match serde_json::from_slice::<SandboxRunnerFailureReport>(&bytes) {
            Ok(report) => RuntimeError::Executor(report.to_error_message()),
            Err(error) => RuntimeError::Executor(format!(
                "{fallback}; failed to parse sandbox failure report '{}': {error}",
                path.display()
            )),
        },
        Err(error) => RuntimeError::Executor(format!(
            "{fallback}; failed to read sandbox failure report '{}': {error}",
            path.display()
        )),
    }
}

pub(super) fn status_message(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => match status.signal() {
            Some(signal) => format!("signal {signal}"),
            None => "unknown status".to_string(),
        },
    }
}

pub(super) fn command_context(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| format!(": {line}"))
        .unwrap_or_default()
}

pub(super) fn add_stderr_context(error: RuntimeError, stderr: &[u8]) -> RuntimeError {
    let context = command_context(stderr);
    if context.is_empty() {
        return error;
    }
    match error {
        RuntimeError::InvalidInput(message) => {
            RuntimeError::InvalidInput(format!("{message}{context}"))
        }
        RuntimeError::Preflight(message) => RuntimeError::Preflight(format!("{message}{context}")),
        RuntimeError::Executor(message) => RuntimeError::Executor(format!("{message}{context}")),
        RuntimeError::Io(error) => RuntimeError::Executor(format!("{error}{context}")),
        RuntimeError::Idmap(error) => RuntimeError::Executor(format!("{error}{context}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsobj_hash::hash_fs_tree_object;
    use mbuild_core::FsTreeEntry;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn setup_failure_context_includes_child_stderr_line() {
        let error = add_stderr_context(
            RuntimeError::Executor("setup failed".to_string()),
            b"\nchild setup detail\nsecond line\n",
        );

        assert!(
            error
                .to_string()
                .contains("setup failed: child setup detail")
        );
    }

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
    fn sandbox_success_report_round_trips_outcome() {
        let temp = tempdir().unwrap();
        let out = temp.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("file"), "contents").unwrap();
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::file("file", 0, 0, 0o644),
        ])
        .unwrap();
        let manifest_jsonl = String::from_utf8(manifest.to_canonical_bytes().unwrap()).unwrap();
        let object_hash = hash_fs_tree_object(manifest_jsonl.as_bytes(), &out).unwrap();
        let path = temp.path().join("success.json");
        let report = SandboxRunnerSuccessReport {
            object_hash: object_hash.to_string(),
            manifest_jsonl,
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

        let outcome = read_sandbox_success_report(&path).unwrap();

        assert_eq!(outcome.object_hash, object_hash);
        assert_eq!(outcome.manifest, manifest);
        assert_eq!(outcome.steps.len(), 1);
        assert_eq!(outcome.steps[0].name, "install");
    }
}
