#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use libcontainer::oci_spec::runtime::Spec;
use libcontainer::workload::{
    Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError,
};
use mbuild_runtime::{
    ExecutorOutcome, MbuildIdmap, RuntimeError, build_ownership_spec, create_bundle,
    run_init_with_executor,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use tempfile::tempdir;
use tracing_subscriber::EnvFilter;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone)]
struct ExitExecutor {
    code: i32,
}

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
fn run_init_reports_success_and_cleans_state() -> TestResult<()> {
    let _guard = runtime_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    init_tracing();
    let workspace = tempdir()?;
    let target_dir = workspace.path().join("target");
    fs::create_dir(&target_dir)?;
    let spec = ownership_spec(&target_dir)?;
    let bundle = create_bundle(workspace.path(), &spec)?;
    let bundle_dir = bundle.dir().to_path_buf();

    let outcome = run_init_with_executor(&bundle, workspace.path(), ExitExecutor { code: 0 })?;

    assert_eq!(outcome, ExecutorOutcome { exit_code: 0 });
    assert_state_root_is_empty(workspace.path())?;
    assert!(bundle_dir.is_dir());

    Ok(())
}

#[test]
fn run_init_reports_executor_failure_and_cleans_state() -> TestResult<()> {
    let _guard = runtime_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    init_tracing();
    let workspace = tempdir()?;
    let target_dir = workspace.path().join("target");
    fs::create_dir(&target_dir)?;
    let spec = ownership_spec(&target_dir)?;
    let bundle = create_bundle(workspace.path(), &spec)?;
    let bundle_dir = bundle.dir().to_path_buf();

    let error = run_init_with_executor(&bundle, workspace.path(), ExitExecutor { code: 7 })
        .expect_err("non-zero executor exit should fail the lifecycle");

    assert!(
        matches!(error, RuntimeError::Executor(_)),
        "expected RuntimeError::Executor, got {error:?}: {error}"
    );
    assert!(error.to_string().contains("Exited"));
    assert_state_root_is_empty(workspace.path())?;
    assert!(bundle_dir.is_dir());

    Ok(())
}

fn runtime_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn ownership_spec(target_dir: &Path) -> Result<Spec, RuntimeError> {
    let idmap = MbuildIdmap::from_host_environment()?;
    build_ownership_spec(&idmap, target_dir)
}

fn assert_state_root_is_empty(workspace: &Path) -> TestResult<()> {
    let state_root = workspace.join("state");
    assert!(state_root.is_dir());
    assert!(fs::read_dir(state_root)?.next().is_none());
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
