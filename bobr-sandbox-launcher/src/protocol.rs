//! Protocol types, constants, validation, and the launcher/runner handshake.
//!
//! Everything here is pure data and helpers shared between the launcher, the
//! runner, and external callers. It must not depend on the `runner` or
//! `launcher` modules.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{self, Write};
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
/// Version 7 replaces the success report with streamed step events.
pub const SANDBOX_PROTOCOL_VERSION: u32 = 7;

const MAX_EVENT_FRAME_LEN: usize = 64 * 1024;

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
    /// Container path where the failure report is written. The runner opens it
    /// after chroot; it is visible on the host because the runtime directory is
    /// bind-mounted (see [`CONTAINER_RUNTIME_DIR`]).
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
    /// When set, the sandbox `root` is established as an overlay of these
    /// layers, mounted before the interior mounts are applied. Writes to the
    /// root land in `upper`, which becomes the captured layer.
    #[serde(default)]
    pub root_overlay: Option<SandboxRootOverlay>,
}

/// Overlay layers that establish the sandbox root.
///
/// The root is mounted as `overlay(lowerdir=lower, upperdir=upper,
/// workdir=work)`. `upper` and `work` must live on the same filesystem, and
/// that filesystem must support the extended attributes overlayfs needs (see
/// the unprivileged `user.overlay.*` xattr namespace).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxRootOverlay {
    /// Read-only lower layer: the input rootfs.
    pub lower: PathBuf,
    /// Writable upper layer: all root writes land here.
    pub upper: PathBuf,
    /// Overlay work directory (kernel-owned scratch, same fs as `upper`).
    pub work: PathBuf,
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

/// Validates a launcher config: every mount target must be a unique, absolute
/// path with no `.`/`..` components (the runner config path is checked too).
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

/// Converts an absolute mount `target` into the path relative to the sandbox
/// root, rejecting non-absolute paths, `.`/`..` components, and `/` itself.
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

/// Converts a path to a [`CString`] for libc calls, failing if it contains a
/// NUL byte.
pub fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains NUL byte: '{}'", path.display()),
        )
    })
}

/// Blocks reading the one-byte readiness handshake from `fd` (retrying on
/// `EINTR`); errors if the pipe closes before a byte arrives.
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

/// Writes the one-byte readiness handshake to `fd` (retrying on `EINTR`).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerRunAs {
    /// Run as the sandbox build user, currently numeric `1:1`.
    BuildUser,
    /// Run as container root, currently numeric `0:0`.
    Root,
}

impl RunnerRunAs {
    /// Stable spelling used in diagnostics and structured build events.
    pub fn as_str(self) -> &'static str {
        match self {
            RunnerRunAs::BuildUser => "build-user",
            RunnerRunAs::Root => "root",
        }
    }
}

/// Intermediate event emitted while the sandbox runner executes build steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SandboxEvent {
    /// A step is about to be spawned.
    StepStarted {
        /// Zero-based index in the configured step list.
        index: usize,
        /// Number of configured steps.
        total: usize,
        /// Step name from the recipe.
        name: String,
        /// Identity used to execute the step.
        run_as: RunnerRunAs,
    },
    /// A step process exited successfully.
    StepFinished {
        /// Zero-based index in the configured step list.
        index: usize,
        /// Number of configured steps.
        total: usize,
        /// Step name from the recipe.
        name: String,
        /// Identity used to execute the step.
        run_as: RunnerRunAs,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
        /// Host-visible path to captured stdout.
        stdout_path: PathBuf,
        /// Host-visible path to captured stderr.
        stderr_path: PathBuf,
    },
    /// A step could not be spawned, waited for, or exited unsuccessfully.
    StepFailed {
        /// Zero-based index in the configured step list.
        index: usize,
        /// Number of configured steps.
        total: usize,
        /// Step name from the recipe.
        name: String,
        /// Identity used to execute the step.
        run_as: RunnerRunAs,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
        /// Exit code, when the process exited normally.
        exit_code: Option<i32>,
        /// Signal number, when the process was killed by a signal.
        signal: Option<i32>,
        /// Host-visible path to captured stdout.
        stdout_path: PathBuf,
        /// Host-visible path to captured stderr.
        stderr_path: PathBuf,
    },
}

/// Writes one length-prefixed JSON sandbox event and flushes it immediately.
pub fn write_sandbox_event(writer: &mut impl Write, event: &SandboxEvent) -> io::Result<()> {
    let payload = serde_json::to_vec(event).map_err(io::Error::other)?;
    if payload.len() > MAX_EVENT_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "sandbox event frame length {} exceeds limit {MAX_EVENT_FRAME_LEN}",
                payload.len()
            ),
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "sandbox event is too large"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

/// Incremental decoder for the runner's length-prefixed event stream.
#[derive(Debug, Default)]
pub struct SandboxEventDecoder {
    bytes: Vec<u8>,
}

impl SandboxEventDecoder {
    /// Adds bytes read from the event pipe.
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Returns the next complete event, or `None` when more bytes are needed.
    pub fn next_event(&mut self) -> io::Result<Option<SandboxEvent>> {
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes(self.bytes[..4].try_into().unwrap()) as usize;
        if length > MAX_EVENT_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sandbox event frame length {length} exceeds limit {MAX_EVENT_FRAME_LEN}"),
            ));
        }
        let frame_len = 4 + length;
        if self.bytes.len() < frame_len {
            return Ok(None);
        }
        let event = serde_json::from_slice(&self.bytes[4..frame_len]).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid sandbox event: {error}"),
            )
        })?;
        self.bytes.drain(..frame_len);
        Ok(Some(event))
    }

    /// Verifies that EOF did not split a frame.
    pub fn finish(&self) -> io::Result<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sandbox event pipe closed with a partial frame",
            ))
        }
    }
}

/// Report describing why the sandbox run failed. The fields beyond
/// `label`/`message` are populated when a specific step process failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRunnerFailureReport {
    /// Short label for the failing phase or step.
    pub label: String,
    /// Human-readable failure message.
    pub message: String,
    /// Exit code, if a step process exited non-zero.
    pub exit_code: Option<i32>,
    /// Terminating signal, if the step was killed by one.
    pub signal: Option<i32>,
    /// Wall-clock duration of the failed step, in milliseconds.
    pub duration_ms: Option<u64>,
    /// Path to the captured stdout log, if any.
    pub stdout_path: Option<PathBuf>,
    /// Path to the captured stderr log, if any.
    pub stderr_path: Option<PathBuf>,
}

impl SandboxRunnerFailureReport {
    /// Builds a failure report for an early/runtime error (no step process
    /// involved): only `label` and `message` are set.
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

    /// Renders the report as a single-line error string, appending whichever
    /// optional fields (exit code, signal, duration, log paths) are set.
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
            root_overlay: None,
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

    #[test]
    fn sandbox_event_decoder_accepts_partial_and_multiple_frames() {
        let first = SandboxEvent::StepStarted {
            index: 0,
            total: 2,
            name: "configure".to_string(),
            run_as: RunnerRunAs::BuildUser,
        };
        let second = SandboxEvent::StepFinished {
            index: 0,
            total: 2,
            name: "configure".to_string(),
            run_as: RunnerRunAs::BuildUser,
            duration_ms: 17,
            stdout_path: PathBuf::from("/logs/stdout"),
            stderr_path: PathBuf::from("/logs/stderr"),
        };
        let mut bytes = Vec::new();
        write_sandbox_event(&mut bytes, &first).unwrap();
        write_sandbox_event(&mut bytes, &second).unwrap();

        let mut decoder = SandboxEventDecoder::default();
        for byte in &bytes[..bytes.len() - 1] {
            decoder.push(std::slice::from_ref(byte));
        }
        assert_eq!(decoder.next_event().unwrap(), Some(first));
        assert_eq!(decoder.next_event().unwrap(), None);

        decoder.push(&bytes[bytes.len() - 1..]);
        assert_eq!(decoder.next_event().unwrap(), Some(second));
        assert_eq!(decoder.next_event().unwrap(), None);
        decoder.finish().unwrap();
    }

    #[test]
    fn sandbox_event_decoder_rejects_partial_and_oversized_frames() {
        let mut partial = SandboxEventDecoder::default();
        partial.push(&[0, 0, 0, 5, b'{']);
        assert_eq!(partial.next_event().unwrap(), None);
        assert_eq!(
            partial.finish().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let mut oversized = SandboxEventDecoder::default();
        oversized.push(&((MAX_EVENT_FRAME_LEN as u32) + 1).to_be_bytes());
        assert_eq!(
            oversized.next_event().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
