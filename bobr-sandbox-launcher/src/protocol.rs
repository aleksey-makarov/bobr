//! Protocol types, constants, validation, and the launcher/runner handshake.
//!
//! Everything here is pure data and helpers shared between the launcher, the
//! runner, and external callers. It must not depend on the `runner` or
//! `launcher` modules.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Executable name expected for the sandbox launcher binary.
pub const LAUNCHER_BINARY_NAME: &str = "bobr-sandbox-launcher";
/// Version of the JSON protocol shared by the sandbox runtime and launcher.
///
/// The runtime writes `RunnerConfig` and `SandboxLauncherConfig` files with
/// this version, and the launcher rejects configs with a different value.
/// Version 6 removes the legacy output scan from the launcher.
pub const SANDBOX_PROTOCOL_VERSION: u32 = 6;

macro_rules! container_bobr_dir {
    () => {
        "/__bobr"
    };
}

macro_rules! container_path {
    ($relative:literal) => {
        concat!(container_bobr_dir!(), "/", $relative)
    };
}

/// Container-private directory used for bobr runtime state.
pub const CONTAINER_BOBR_DIR: &str = container_bobr_dir!();
/// Container path where the build working directory is mounted.
pub const CONTAINER_BUILD_DIR: &str = container_path!("build");
/// Container path where generated step configuration is mounted.
pub const CONTAINER_CONFIG_DIR: &str = container_path!("config");
/// Container path containing named input object mounts.
pub const CONTAINER_INPUTS_DIR: &str = container_path!("inputs");
/// Container path used for step log files.
pub const CONTAINER_LOG_DIR: &str = container_path!("logs");
/// Container path where build output is collected.
pub const CONTAINER_OUT_DIR: &str = container_path!("out");
/// Container path containing the copied launcher executable.
pub const CONTAINER_LAUNCHER_DIR: &str = container_path!("launcher");
/// Container path containing runner protocol files.
pub const CONTAINER_RUNTIME_DIR: &str = container_path!("runtime");
/// Container path of the runner config JSON file.
pub const CONTAINER_RUNNER_CONFIG: &str = container_path!("runtime/runner-config.json");
/// Container path of the success report JSON file.
pub const CONTAINER_SUCCESS_REPORT: &str = container_path!("runtime/sandbox-success.json");
/// Container path of the failure report JSON file.
pub const CONTAINER_FAILURE_REPORT: &str = container_path!("runtime/sandbox-failure.json");

/// Protocol information printed by the launcher during preflight checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LauncherProtocolInfo {
    /// Launcher binary name.
    pub name: String,
    /// Sandbox protocol version.
    pub protocol_version: u32,
}

/// Returns the protocol information expected by the runtime.
pub fn protocol_info() -> LauncherProtocolInfo {
    LauncherProtocolInfo {
        name: LAUNCHER_BINARY_NAME.to_string(),
        protocol_version: SANDBOX_PROTOCOL_VERSION,
    }
}

/// Internal JSON config consumed by the sandbox runner inside the namespace.
///
/// This is not a recipe-facing format. The sandbox parent writes it after it has
/// prepared the sandbox filesystem and resolved recipe-level sandbox settings
/// into concrete container paths, environment variables, and step policies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// Sandbox protocol version. Must match [`SANDBOX_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Container paths that the runner creates before executing steps.
    pub prepare_paths: Vec<PathBuf>,
    /// Ordered command steps to run.
    pub steps: Vec<RunnerStepConfig>,
    /// Container path where the success report is written. The runner opens it
    /// after chroot; it is visible on the host because the runtime directory is
    /// bind-mounted (see [`CONTAINER_RUNTIME_DIR`]).
    pub success_report: PathBuf,
    /// Container path where the failure report is written. Same handoff as
    /// `success_report`: opened by the runner after chroot, visible on the host
    /// via the bind-mounted runtime directory.
    pub failure_report: PathBuf,
}

/// JSON config consumed by the launcher before it runs the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxLauncherConfig {
    /// Sandbox protocol version. Must match [`SANDBOX_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Host path used as the sandbox root.
    pub root: PathBuf,
    /// Mounts to create in the sandbox mount namespace.
    pub mounts: Vec<SandboxLauncherMount>,
    /// Container path of the runner config JSON file.
    pub runner_config: PathBuf,
    /// Host-visible path where launcher failures are reported.
    pub failure_report: PathBuf,
}

/// One mount entry in [`SandboxLauncherConfig`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxLauncherMount {
    /// Mount operation kind.
    pub kind: SandboxLauncherMountKind,
    /// Host source path for bind mounts.
    pub source: Option<PathBuf>,
    /// Absolute target path inside the sandbox.
    pub target: PathBuf,
    /// Whether the mount is remounted read-only.
    pub readonly: bool,
    /// Extra mount options.
    pub options: Vec<String>,
}

/// Mount operation kind used by the sandbox launcher.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLauncherMountKind {
    /// Bind mount from a host source path.
    Bind,
    /// Procfs mount.
    Proc,
    /// Tmpfs mount.
    Tmpfs,
}

pub fn validate_launcher_config(config: &SandboxLauncherConfig) -> io::Result<()> {
    let mut targets = HashSet::new();
    for mount in &config.mounts {
        let relative = relative_launcher_target(&mount.target)?;
        if !targets.insert(relative) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "mount target '{}' is defined more than once",
                    mount.target.display()
                ),
            ));
        }
    }
    relative_launcher_target(&config.runner_config)?;
    Ok(())
}

pub fn relative_launcher_target(target: &Path) -> io::Result<PathBuf> {
    if !target.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mount target '{}' must be absolute", target.display()),
        ));
    }
    if target
        .as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .any(|segment| segment == b"." || segment == b"..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "mount target '{}' must not contain '.' or '..' components",
                target.display()
            ),
        ));
    }
    let mut relative = PathBuf::new();
    for component in target.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(segment) => relative.push(segment),
            Component::CurDir | Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "mount target '{}' must not contain '.' or '..' components",
                        target.display()
                    ),
                ));
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "mount target '{}' contains an unsupported path component",
                        target.display()
                    ),
                ));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mount target must not be '/'",
        ));
    }
    Ok(relative)
}

pub fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains NUL byte: '{}'", path.display()),
        )
    })
}

pub fn read_handshake_byte(fd: RawFd) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    loop {
        let result = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if result == 1 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "handshake pipe closed before signalling readiness",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub fn write_handshake_byte(fd: RawFd) -> io::Result<()> {
    let byte = [1_u8; 1];
    loop {
        let result = unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) };
        if result == 1 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write handshake byte",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// One command step in the internal runner config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerStepConfig {
    /// Step name used in reports and diagnostics.
    pub name: String,
    /// User identity used to execute the step.
    pub run_as: RunnerRunAs,
    /// Absolute working directory inside the sandbox.
    pub cwd: PathBuf,
    /// Argument vector to execute.
    pub argv: Vec<String>,
    /// Complete process environment for the step.
    ///
    /// This is already the effective environment written by the runtime, not
    /// only recipe-level overrides.
    pub env: HashMap<String, String>,
    /// File creation mask applied immediately before executing the step.
    ///
    /// This field is required by the current protocol
    /// ([`SANDBOX_PROTOCOL_VERSION`]). Values must be in `0o000..=0o777`;
    /// invalid values make the runner reject the config.
    pub umask: u32,
    /// Container path where step stdout is captured.
    pub stdout_path: PathBuf,
    /// Container path where step stderr is captured.
    pub stderr_path: PathBuf,
    /// Host-visible stdout path recorded in the step report.
    pub report_stdout_path: PathBuf,
    /// Host-visible stderr path recorded in the step report.
    pub report_stderr_path: PathBuf,
}

/// User identity used to execute one runner step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerRunAs {
    /// Run as the sandbox build user, currently numeric `1:1`.
    BuildUser,
    /// Run as container root, currently numeric `0:0`.
    Root,
}

impl RunnerRunAs {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RunnerRunAs::BuildUser => "build-user",
            RunnerRunAs::Root => "root",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxStepReport {
    pub name: String,
    pub run_as: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SandboxRunnerSuccessReport {
    pub steps: Vec<SandboxStepReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRunnerFailureReport {
    pub label: String,
    pub message: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub duration_ms: Option<u128>,
    pub stdout_path: Option<PathBuf>,
    pub stderr_path: Option<PathBuf>,
}

impl SandboxRunnerFailureReport {
    pub fn runtime(label: &str, message: String) -> Self {
        Self {
            label: label.to_string(),
            message,
            exit_code: None,
            signal: None,
            duration_ms: None,
            stdout_path: None,
            stderr_path: None,
        }
    }

    pub fn to_error_message(&self) -> String {
        let mut message = format!("{}: {}", self.label, self.message);
        if let Some(code) = self.exit_code {
            message.push_str(&format!("; exit_status={code}"));
        }
        if let Some(signal) = self.signal {
            message.push_str(&format!("; signal={signal}"));
        }
        if let Some(duration_ms) = self.duration_ms {
            message.push_str(&format!("; duration_ms={duration_ms}"));
        }
        if let Some(stdout) = &self.stdout_path {
            message.push_str(&format!("; stdout={}", stdout.display()));
        }
        if let Some(stderr) = &self.stderr_path {
            message.push_str(&format!("; stderr={}", stderr.display()));
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    struct Pipe {
        read: OwnedFd,
        write: OwnedFd,
    }

    impl Pipe {
        fn new() -> io::Result<Self> {
            let mut fds = [0; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                read: unsafe { OwnedFd::from_raw_fd(fds[0]) },
                write: unsafe { OwnedFd::from_raw_fd(fds[1]) },
            })
        }
    }

    #[test]
    fn relative_launcher_target_rejects_unsafe_paths() {
        for target in ["relative", "/", "/tmp/../out", "/tmp/./out"] {
            assert!(
                relative_launcher_target(Path::new(target)).is_err(),
                "{target} should be rejected"
            );
        }
        assert_eq!(
            relative_launcher_target(Path::new(CONTAINER_OUT_DIR)).unwrap(),
            PathBuf::from("__bobr/out")
        );
    }

    #[test]
    fn path_cstring_rejects_nul_paths() {
        let path = Path::new("bad\0path");

        let error = path_cstring(path).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("NUL"));
    }

    #[test]
    fn read_handshake_byte_reads_one_byte() {
        let pipe = Pipe::new().unwrap();
        write_handshake_byte(pipe.write.as_raw_fd()).unwrap();

        read_handshake_byte(pipe.read.as_raw_fd()).unwrap();
    }

    #[test]
    fn read_handshake_byte_reports_closed_writer() {
        let pipe = Pipe::new().unwrap();
        drop(pipe.write);

        let error = read_handshake_byte(pipe.read.as_raw_fd()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn write_handshake_byte_writes_readable_byte() {
        let pipe = Pipe::new().unwrap();
        let mut byte = [0_u8; 1];

        write_handshake_byte(pipe.write.as_raw_fd()).unwrap();
        let read = unsafe { libc::read(pipe.read.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };

        assert_eq!(read, 1);
        assert_eq!(byte, [1]);
    }

    #[test]
    fn validate_launcher_config_rejects_duplicate_mount_targets() {
        let config = SandboxLauncherConfig {
            protocol_version: SANDBOX_PROTOCOL_VERSION,
            root: PathBuf::from("/tmp/root"),
            mounts: vec![
                SandboxLauncherMount {
                    kind: SandboxLauncherMountKind::Tmpfs,
                    source: None,
                    target: PathBuf::from("/tmp"),
                    readonly: false,
                    options: Vec::new(),
                },
                SandboxLauncherMount {
                    kind: SandboxLauncherMountKind::Tmpfs,
                    source: None,
                    target: PathBuf::from("/tmp/"),
                    readonly: false,
                    options: Vec::new(),
                },
            ],
            runner_config: PathBuf::from(CONTAINER_RUNNER_CONFIG),
            failure_report: PathBuf::from("/tmp/failure.json"),
        };

        let error = validate_launcher_config(&config).unwrap_err();

        assert!(error.to_string().contains("defined more than once"));
    }

    #[test]
    fn protocol_info_reports_current_launcher_protocol() {
        let info = protocol_info();

        assert_eq!(info.name, LAUNCHER_BINARY_NAME);
        assert_eq!(info.protocol_version, SANDBOX_PROTOCOL_VERSION);
    }
}
