//! `libcontainer` run lifecycle helpers.

use crate::{Bundle, RuntimeError};
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::syscall::syscall::SyscallType;
use nix::sys::wait::{WaitStatus, waitpid};
use std::fs;
use std::path::Path;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorOutcome {
    pub exit_code: i32,
}

pub fn run_init_with_executor<E>(
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

    let mut result = run_container(bundle, &state_dir, &suffix, executor);

    if let Err(error) = fs::remove_dir_all(&state_dir) {
        if result.is_ok() {
            result = Err(RuntimeError::Io(error));
        } else {
            warn!(
                "failed to remove runtime state directory '{}': {error}",
                state_dir.display()
            );
        }
    }

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

fn libcontainer_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Libcontainer(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

    fn test_bundle(workspace: &Path) -> Bundle {
        let spec = crate::build_ownership_spec(
            &crate::MbuildIdmap::for_tests(1000, 1001, 100000, 65536, 200000, 65536),
            Path::new("/tmp/mbuild-runtime-target"),
        )
        .unwrap();

        crate::create_bundle(workspace, &spec).unwrap()
    }
}
