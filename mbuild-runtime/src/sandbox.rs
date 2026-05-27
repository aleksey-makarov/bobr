//! Runtime-backed sandbox build execution.

mod config;
mod lifecycle;
mod mounts;
mod reports;
mod tools;

use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use lifecycle::SandboxLifecycle;
use mounts::PreparedSandbox;
use tracing::warn;

pub use config::{SandboxBuildConfig, SandboxInput, SandboxRunAs, SandboxStep};
pub use reports::{SandboxBuildOutcome, SandboxStepReport};

const CONTAINER_RUNTIME_DIR: &str = "/__mbuild/runtime";
const CONTAINER_RUNNER_DIR: &str = "/__mbuild/runner";
const CONTAINER_LOG_DIR: &str = "/__mbuild/logs";
const CONTAINER_RUNNER_CONFIG: &str = "/__mbuild/runtime/runner-config.json";
const CONTAINER_SUCCESS_REPORT: &str = "/__mbuild/runtime/sandbox-success.json";
const CONTAINER_FAILURE_REPORT: &str = "/__mbuild/runtime/sandbox-failure.json";
const CONTAINER_BREADCRUMBS: &str = "/__mbuild/runtime/sandbox-breadcrumbs.log";

/// Execute a complete sandbox build and return the output hash.
pub fn run_sandbox_build(
    config: SandboxBuildConfig,
    idmap: &MbuildIdmap,
) -> Result<SandboxBuildOutcome, RuntimeError> {
    config::validate_config(&config)?;
    crate::preflight::preflight_local_helper_runtime(idmap)?;
    let tools = tools::cached_sandbox_tools()?;

    let prepared = PreparedSandbox::create(&config, &tools.runner.host_path)?;
    let mut lifecycle = SandboxLifecycle::start(&tools, idmap, prepared)?;
    let result = lifecycle.wait_for_outcome();
    let cleanup = lifecycle.cleanup();

    match (result, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(outcome), Err(error)) => {
            warn!("failed to cleanup sandbox runtime after successful build: {error}");
            Ok(outcome)
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            warn!("failed to cleanup sandbox runtime after failed build: {cleanup_error}");
            Err(error)
        }
    }
}
