//! Shared types and implementation for the sandbox launcher protocol.
//!
//! The current JSON format is launcher protocol v6. Protocol v6 removes the
//! legacy output scan from the launcher; callers own output import/reporting.
//!
//! Modules:
//! - `protocol`: shared types, constants, validation, and the handshake.
//! - `runner`: the in-namespace runner that executes steps as pid 1.
//! - `launcher`: the privileged setup (namespaces, mounts, caps, chroot).

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod launcher;
mod protocol;
mod runner;

pub use launcher::launch;
pub use protocol::{
    CONTAINER_BOBR_DIR, CONTAINER_BUILD_DIR, CONTAINER_CONFIG_DIR, CONTAINER_FAILURE_REPORT,
    CONTAINER_INPUTS_DIR, CONTAINER_LAUNCHER_DIR, CONTAINER_LOG_DIR, CONTAINER_OUT_DIR,
    CONTAINER_RUNNER_CONFIG, CONTAINER_RUNTIME_DIR, CONTAINER_SUCCESS_REPORT, LAUNCHER_BINARY_NAME,
    LauncherProtocolInfo, RunnerConfig, RunnerRunAs, RunnerStepConfig, SANDBOX_PROTOCOL_VERSION,
    SandboxLauncherConfig, SandboxLauncherMount, SandboxLauncherMountKind,
    SandboxRunnerFailureReport, SandboxRunnerSuccessReport, SandboxStepReport, path_cstring,
    protocol_info, read_handshake_byte, relative_launcher_target, validate_launcher_config,
    write_handshake_byte,
};
pub use runner::{RunnerOutcome, run_config, run_config_path};
