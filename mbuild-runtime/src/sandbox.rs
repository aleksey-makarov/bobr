//! Runtime-backed sandbox build execution.

mod config;
mod lifecycle;
mod mounts;
mod reports;
mod tools;

use crate::error::RuntimeError;
use crate::idmap::cached_host_idmap;
use lifecycle::SandboxLifecycle;
use mounts::PreparedSandbox;
use tracing::warn;

pub use config::{SandboxBuildConfig, SandboxInput, SandboxRunAs, SandboxStep};
pub use reports::{SandboxBuildOutcome, SandboxStepReport};

/// Execute a complete sandbox build and return the output hash.
pub fn run_sandbox_build(config: SandboxBuildConfig) -> Result<SandboxBuildOutcome, RuntimeError> {
    config::validate_config(&config)?;
    let idmap = cached_host_idmap()
        .map_err(|error| RuntimeError::Preflight(format!("failed to load host idmap: {error}")))?;
    crate::preflight::preflight_local_helper_runtime(idmap.as_ref())?;
    let tools = tools::cached_sandbox_tools()?;

    let prepared = PreparedSandbox::create(&config, &tools.runner.host_path)?;
    let mut lifecycle = SandboxLifecycle::start(&tools, idmap.as_ref(), prepared)?;
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
