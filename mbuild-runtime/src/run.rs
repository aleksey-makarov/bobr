//! `libcontainer` run lifecycle helpers.

use crate::{bundle::Bundle, error::RuntimeError, executor::read_executor_error_report};
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::syscall::syscall::SyscallType;
use nix::sys::wait::{WaitStatus, waitpid};
use std::fs;
use std::path::Path;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutorOutcome {
    pub(crate) exit_code: i32,
}

pub(crate) fn run_init_with_executor<E>(
    bundle: &Bundle,
    workspace: &Path,
    executor: E,
) -> Result<ExecutorOutcome, RuntimeError>
where
    E: libcontainer::workload::Executor + 'static,
{
    if !workspace.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "runtime run workspace '{}' must exist and be a directory",
            workspace.display()
        )));
    }

    let state_root = workspace.join("state");
    fs::create_dir_all(&state_root)?;

    let suffix = Uuid::new_v4().simple().to_string();
    let state_dir = state_root.join(&suffix);
    fs::create_dir(&state_dir)?;

    let lifecycle_result = run_container(bundle, &state_dir, &suffix, executor);
    let mut result = resolve_executor_report(bundle.error_log_path(), lifecycle_result);

    cleanup_state_dir(&state_dir, &mut result);

    result
}

fn run_container<E>(
    bundle: &Bundle,
    state_dir: &Path,
    suffix: &str,
    executor: E,
) -> Result<ExecutorOutcome, RuntimeError>
where
    E: libcontainer::workload::Executor + 'static,
{
    let container_id = format!("mbuild-runtime-{suffix}");
    let mut container = ContainerBuilder::new(container_id, SyscallType::Linux)
        .with_executor(executor)
        .with_root_path(state_dir)
        .map_err(libcontainer_error)?
        .as_init(bundle.dir())
        .with_systemd(false)
        .with_detach(false)
        .build()
        .map_err(libcontainer_error)?;

    let mut result = match container.pid() {
        Some(pid) => {
            let start_result = container.start();
            let wait_result = waitpid(pid, None);

            if let Err(error) = start_result {
                Err(libcontainer_error(error))
            } else {
                match wait_result {
                    Ok(status) => wait_status_outcome(status),
                    Err(error) => Err(libcontainer_error(error)),
                }
            }
        }
        None => Err(RuntimeError::Libcontainer(
            "libcontainer did not expose init pid".to_string(),
        )),
    };

    if let Err(error) = container.delete(true) {
        if result.is_ok() {
            result = Err(libcontainer_error(error));
        } else {
            warn!("failed to delete runtime container: {error}");
        }
    }

    result
}

fn wait_status_outcome(status: WaitStatus) -> Result<ExecutorOutcome, RuntimeError> {
    match status {
        WaitStatus::Exited(_, 0) => Ok(ExecutorOutcome { exit_code: 0 }),
        status => Err(RuntimeError::Executor(format!(
            "init executor wait status was {status:?}"
        ))),
    }
}

fn resolve_executor_report(
    path: &Path,
    lifecycle_result: Result<ExecutorOutcome, RuntimeError>,
) -> Result<ExecutorOutcome, RuntimeError> {
    match read_executor_error_report(path) {
        Ok(Some(report)) => Err(RuntimeError::Executor(report.to_string())),
        Ok(None) => lifecycle_result,
        Err(RuntimeError::Executor(message)) => match lifecycle_result {
            Ok(_) => Err(RuntimeError::Executor(message)),
            Err(lifecycle_error) => Err(RuntimeError::Executor(format!(
                "{message}; lifecycle error was: {lifecycle_error}"
            ))),
        },
        Err(error) => Err(error),
    }
}

fn cleanup_state_dir(state_dir: &Path, result: &mut Result<ExecutorOutcome, RuntimeError>) {
    if let Err(error) = fs::remove_dir_all(state_dir) {
        if result.is_ok() {
            *result = Err(RuntimeError::Io(error));
        } else {
            warn!(
                "failed to remove runtime state directory '{}': {error}",
                state_dir.display()
            );
        }
    }
}

fn libcontainer_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Libcontainer(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    use libcontainer::oci_spec::runtime::Spec;
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    use libcontainer::workload::{
        Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError,
    };
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    use std::collections::HashMap;
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    use tracing_subscriber::EnvFilter;

    #[test]
    fn run_init_rejects_missing_workspace() {
        let workspace = tempdir().unwrap();
        let missing = workspace.path().join("missing");
        let bundle = test_bundle(workspace.path());
        let error = run_init_with_executor(&bundle, &missing, NoopExecutor).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(!missing.exists());
    }

    #[test]
    fn run_init_rejects_non_directory_workspace() {
        let workspace = tempdir().unwrap();
        let file = workspace.path().join("workspace-file");
        fs::write(&file, b"not a directory").unwrap();
        let bundle = test_bundle(workspace.path());
        let error = run_init_with_executor(&bundle, &file, NoopExecutor).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(!file.join("state").exists());
    }

    #[test]
    fn resolve_executor_report_keeps_success_when_report_is_empty() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("error.json");
        fs::write(&path, b"").unwrap();

        let outcome = resolve_executor_report(&path, Ok(ExecutorOutcome { exit_code: 0 }))
            .expect("empty report should preserve lifecycle success");

        assert_eq!(outcome, ExecutorOutcome { exit_code: 0 });
    }

    #[test]
    fn resolve_executor_report_keeps_lifecycle_error_when_report_is_empty() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("error.json");
        fs::write(&path, b"").unwrap();

        let error = resolve_executor_report(
            &path,
            Err(RuntimeError::Executor(
                "init executor wait status was Exited(1, 7)".to_string(),
            )),
        )
        .unwrap_err();

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert!(
            error
                .to_string()
                .contains("init executor wait status was Exited(1, 7)")
        );
    }

    #[test]
    fn resolve_executor_report_turns_success_with_report_into_executor_error() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("error.json");
        write_report(&path);

        let error = resolve_executor_report(&path, Ok(ExecutorOutcome { exit_code: 0 }))
            .expect_err("non-empty report should override lifecycle success");

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert_eq!(
            error.to_string(),
            "executor error: chown error at /target/etc/passwd: failed to chown (errno 1)"
        );
    }

    #[test]
    fn resolve_executor_report_prefers_report_over_lifecycle_error() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("error.json");
        write_report(&path);

        let error = resolve_executor_report(
            &path,
            Err(RuntimeError::Executor(
                "init executor wait status was Exited(1, 7)".to_string(),
            )),
        )
        .unwrap_err();

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert_eq!(
            error.to_string(),
            "executor error: chown error at /target/etc/passwd: failed to chown (errno 1)"
        );
    }

    #[test]
    fn resolve_executor_report_preserves_lifecycle_error_context_for_malformed_report() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("error.json");
        fs::write(&path, b"not json").unwrap();

        let error = resolve_executor_report(
            &path,
            Err(RuntimeError::Executor(
                "init executor wait status was Exited(1, 7)".to_string(),
            )),
        )
        .unwrap_err();

        assert!(matches!(error, RuntimeError::Executor(_)));
        assert!(
            error
                .to_string()
                .contains("failed to parse executor error report")
        );
        assert!(
            error
                .to_string()
                .contains("lifecycle error was: executor error: init executor wait status")
        );
    }

    #[test]
    fn cleanup_state_dir_does_not_override_existing_executor_error() {
        let workspace = tempdir().unwrap();
        let state_dir_file = workspace.path().join("state-dir-file");
        fs::write(&state_dir_file, b"not a directory").unwrap();
        let mut result = Err(RuntimeError::Executor(
            "chown error at /target/etc/passwd: failed to chown".to_string(),
        ));

        cleanup_state_dir(&state_dir_file, &mut result);

        let error = result.unwrap_err();
        assert!(matches!(error, RuntimeError::Executor(_)));
        assert!(
            error
                .to_string()
                .contains("chown error at /target/etc/passwd")
        );
    }

    #[derive(Clone)]
    struct NoopExecutor;

    impl libcontainer::workload::Executor for NoopExecutor {
        fn setup_envs(
            &self,
            _: std::collections::HashMap<String, String>,
        ) -> Result<(), libcontainer::workload::ExecutorSetEnvsError> {
            Ok(())
        }

        fn validate(
            &self,
            _: &libcontainer::oci_spec::runtime::Spec,
        ) -> Result<(), libcontainer::workload::ExecutorValidationError> {
            Ok(())
        }

        fn exec(
            &self,
            _: &libcontainer::oci_spec::runtime::Spec,
        ) -> Result<(), libcontainer::workload::ExecutorError> {
            Ok(())
        }
    }

    #[derive(Clone)]
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    struct ExitExecutor {
        code: i32,
    }

    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    impl Executor for ExitExecutor {
        fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
            Ok(())
        }

        fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
            Ok(())
        }

        fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
            std::process::exit(self.code);
        }
    }

    #[test]
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    fn run_init_reports_success_and_cleans_state() {
        let _guard = runtime_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        init_tracing();
        let workspace = tempdir().unwrap();
        let target_dir = workspace.path().join("target");
        fs::create_dir(&target_dir).unwrap();
        let spec = runtime_ownership_spec(&target_dir);
        let bundle = crate::bundle::create_bundle(workspace.path(), &spec).unwrap();
        let bundle_dir = bundle.dir().to_path_buf();

        let outcome =
            run_init_with_executor(&bundle, workspace.path(), ExitExecutor { code: 0 }).unwrap();

        assert_eq!(outcome, ExecutorOutcome { exit_code: 0 });
        assert_state_root_is_empty(workspace.path());
        assert!(bundle_dir.is_dir());
    }

    #[test]
    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    fn run_init_reports_executor_failure_and_cleans_state() {
        let _guard = runtime_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        init_tracing();
        let workspace = tempdir().unwrap();
        let target_dir = workspace.path().join("target");
        fs::create_dir(&target_dir).unwrap();
        let spec = runtime_ownership_spec(&target_dir);
        let bundle = crate::bundle::create_bundle(workspace.path(), &spec).unwrap();
        let bundle_dir = bundle.dir().to_path_buf();

        let error = run_init_with_executor(&bundle, workspace.path(), ExitExecutor { code: 7 })
            .expect_err("non-zero executor exit should fail the lifecycle");

        assert!(
            matches!(error, RuntimeError::Executor(_)),
            "expected RuntimeError::Executor, got {error:?}: {error}"
        );
        assert!(error.to_string().contains("Exited"));
        assert_state_root_is_empty(workspace.path());
        assert!(bundle_dir.is_dir());
    }

    fn test_bundle(workspace: &Path) -> Bundle {
        let spec = crate::spec::build_ownership_spec(
            &crate::idmap::MbuildIdmap::for_tests(1000, 1001, 100000, 65536, 200000, 65536),
            Path::new("/tmp/mbuild-runtime-target"),
        )
        .unwrap();

        crate::bundle::create_bundle(workspace, &spec).unwrap()
    }

    fn write_report(path: &Path) {
        crate::executor::write_executor_error_report(
            path,
            &crate::executor::ExecutorErrorReport {
                kind: "chown".to_string(),
                path: "/target/etc/passwd".to_string(),
                message: "failed to chown".to_string(),
                errno: Some(1),
            },
        )
        .unwrap();
    }

    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    fn runtime_ownership_spec(target_dir: &Path) -> Spec {
        let idmap = crate::idmap::MbuildIdmap::from_host_environment().unwrap();
        crate::spec::build_ownership_spec(&idmap, target_dir).unwrap()
    }

    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    fn assert_state_root_is_empty(workspace: &Path) {
        let state_root = workspace.join("state");
        assert!(state_root.is_dir());
        assert!(fs::read_dir(state_root).unwrap().next().is_none());
    }

    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    fn runtime_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[cfg(all(feature = "integration-tests", target_os = "linux"))]
    fn init_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    }
}
