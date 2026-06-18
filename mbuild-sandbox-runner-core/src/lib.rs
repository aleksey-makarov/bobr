//! Shared types and implementation for the sandbox runner protocol.
//!
//! The current JSON format is runner protocol v5. Protocol v5 requires an
//! explicit output mode in addition to the per-step `umask` field introduced
//! by protocol v4.

use fsobj_hash::{hash_fs_tree_object, hash_path, hash_symlink_node};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
#[cfg(not(test))]
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
#[cfg(not(test))]
use nix::unistd::Pid;
use nix::unistd::{Gid, Uid, chown, setgid, setgroups, setuid};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Instant;

/// Executable name expected for the sandbox runner binary.
pub const RUNNER_BINARY_NAME: &str = "mbuild-sandbox-runner";
/// Version of the JSON protocol shared by the sandbox runtime and runner.
///
/// The runtime writes `RunnerConfig` and `SandboxLauncherConfig` files with
/// this version, and the runner rejects configs with a different value.
/// Version 5 requires an explicit runner output mode.
pub const RUNNER_PROTOCOL_VERSION: u32 = 5;

macro_rules! container_mbuild_dir {
    () => {
        "/__mbuild"
    };
}

macro_rules! container_path {
    ($relative:literal) => {
        concat!(container_mbuild_dir!(), "/", $relative)
    };
}

/// Container-private directory used for mbuild runtime state.
pub const CONTAINER_MBUILD_DIR: &str = container_mbuild_dir!();
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
/// Container path containing the copied runner executable.
pub const CONTAINER_RUNNER_DIR: &str = container_path!("runner");
/// Container path containing runner protocol files.
pub const CONTAINER_RUNTIME_DIR: &str = container_path!("runtime");
/// Container path of the runner config JSON file.
pub const CONTAINER_RUNNER_CONFIG: &str = container_path!("runtime/runner-config.json");
/// Container path of the success report JSON file.
pub const CONTAINER_SUCCESS_REPORT: &str = container_path!("runtime/sandbox-success.json");
/// Container path of the failure report JSON file.
pub const CONTAINER_FAILURE_REPORT: &str = container_path!("runtime/sandbox-failure.json");

const BUILD_USER_UID: u32 = 1;
const BUILD_USER_GID: u32 = 1;

/// Protocol information printed by the runner during preflight checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerProtocolInfo {
    /// Runner binary name.
    pub name: String,
    /// Runner protocol version.
    pub protocol_version: u32,
}

/// Returns the protocol information expected by the runtime.
pub fn protocol_info() -> RunnerProtocolInfo {
    RunnerProtocolInfo {
        name: RUNNER_BINARY_NAME.to_string(),
        protocol_version: RUNNER_PROTOCOL_VERSION,
    }
}

/// Internal JSON config consumed by the sandbox runner inside the namespace.
///
/// This is not a recipe-facing format. `mbuild-runtime` writes it after it has
/// prepared the sandbox filesystem and resolved recipe-level sandbox settings
/// into concrete container paths, environment variables, and step policies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// Runner protocol version. Must match [`RUNNER_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Container paths that the runner creates before executing steps.
    pub prepare_paths: Vec<PathBuf>,
    /// Ordered command steps to run.
    pub steps: Vec<RunnerStepConfig>,
    /// Container path where final output is expected.
    pub output_dir: PathBuf,
    /// Output reporting mode used after all steps have completed.
    pub output_mode: RunnerOutputMode,
    /// Host-visible path where the success report is written.
    pub success_report: PathBuf,
    /// Host-visible path where the failure report is written.
    pub failure_report: PathBuf,
}

/// Post-step output handling performed by the runner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerOutputMode {
    /// Scan the output directory as a legacy fs-tree object and report its
    /// manifest/hash in the success report.
    LegacyFsTreeReport,
    /// Report only step status. The caller owns any output scan/import.
    StepsOnly,
}

/// JSON config consumed by the launcher before it execs the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxLauncherConfig {
    /// Runner protocol version. Must match [`RUNNER_PROTOCOL_VERSION`].
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
    /// This field is required since runner protocol v4. Values must be in
    /// `0o000..=0o777`; invalid values make the runner reject the config.
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
    fn as_str(self) -> &'static str {
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
    pub legacy: Option<SandboxRunnerLegacyOutput>,
    pub steps: Vec<SandboxStepReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRunnerLegacyOutput {
    pub object_hash: String,
    pub manifest_jsonl: String,
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

    fn step_runtime(step: &SandboxRunnerStep, message: String, duration_ms: u128) -> Self {
        Self {
            label: step.name.clone(),
            message,
            exit_code: None,
            signal: None,
            duration_ms: Some(duration_ms),
            stdout_path: Some(step.report_stdout_path.clone()),
            stderr_path: Some(step.report_stderr_path.clone()),
        }
    }

    fn failed_step(step: &SandboxRunnerStep, status: &ExitStatus, duration_ms: u128) -> Self {
        let (message, exit_code, signal) = if let Some(code) = status.code() {
            (
                format!(
                    "sandbox step '{}' failed with exit status {code}",
                    step.name
                ),
                Some(code),
                None,
            )
        } else if let Some(signal) = status.signal() {
            (
                format!("sandbox step '{}' was killed by signal {signal}", step.name),
                None,
                Some(signal),
            )
        } else {
            (
                format!("sandbox step '{}' ended with status {status:?}", step.name),
                None,
                None,
            )
        };
        Self {
            label: step.name.clone(),
            message,
            exit_code,
            signal,
            duration_ms: Some(duration_ms),
            stdout_path: Some(step.report_stdout_path.clone()),
            stderr_path: Some(step.report_stderr_path.clone()),
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

pub fn run_config_path(path: &Path) -> i32 {
    let config = match fs::read(path)
        .map_err(|error| format!("failed to read runner config '{}': {error}", path.display()))
        .and_then(|bytes| serde_json::from_slice::<RunnerConfig>(&bytes).map_err(|e| e.to_string()))
    {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };

    run_config(config)
}

pub fn run_config(config: RunnerConfig) -> i32 {
    if config.protocol_version != RUNNER_PROTOCOL_VERSION {
        let report = SandboxRunnerFailureReport::runtime(
            "runner-protocol",
            format!(
                "unsupported runner protocol {}; expected {}",
                config.protocol_version, RUNNER_PROTOCOL_VERSION
            ),
        );
        if let Ok(file) = File::create(&config.failure_report) {
            let _ = serde_json::to_writer(file, &report);
        }
        return 1;
    }
    let runner = match SandboxRunner::new(config) {
        Ok(runner) => runner,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    runner.exec()
}

struct SandboxRunner {
    prepare_paths: Vec<PathBuf>,
    steps: Vec<SandboxRunnerStep>,
    output_dir: PathBuf,
    output_mode: RunnerOutputMode,
    success_report: Arc<File>,
    failure_report: Arc<File>,
}

type RunnerResult<T> = Result<T, Box<SandboxRunnerFailureReport>>;

fn runner_error(report: SandboxRunnerFailureReport) -> Box<SandboxRunnerFailureReport> {
    Box::new(report)
}

impl SandboxRunner {
    fn new(config: RunnerConfig) -> io::Result<Self> {
        let success_report = Arc::new(File::create(&config.success_report)?);
        let failure_report = Arc::new(File::create(&config.failure_report)?);
        let steps = config
            .steps
            .into_iter()
            .map(SandboxRunnerStep::new)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            prepare_paths: config.prepare_paths,
            steps,
            output_dir: config.output_dir,
            output_mode: config.output_mode,
            success_report,
            failure_report,
        })
    }

    fn exec(&self) -> i32 {
        let exit_code = match self.run() {
            Ok(outcome) => {
                if let Err(error) = self.write_success(&outcome) {
                    eprintln!("{error}");
                    1
                } else {
                    0
                }
            }
            Err(report) => {
                if let Err(error) = self.write_failure(&report) {
                    eprintln!("{error}");
                }
                1
            }
        };
        terminate_remaining_children();
        exit_code
    }

    fn run(&self) -> RunnerResult<SandboxRunnerOutcome> {
        self.prepare()?;

        let mut reports = Vec::new();
        for step in &self.steps {
            reports.push(step.run()?);
        }

        let legacy = match self.output_mode {
            RunnerOutputMode::LegacyFsTreeReport => {
                let manifest = scan_output_manifest(&self.output_dir)?;
                let manifest_bytes = manifest.to_canonical_bytes().map_err(|error| {
                    runner_error(SandboxRunnerFailureReport::runtime(
                        "sandbox-manifest-output",
                        error.to_string(),
                    ))
                })?;
                let object_hash =
                    hash_fs_tree_object(&manifest_bytes, &self.output_dir).map_err(|error| {
                        runner_error(SandboxRunnerFailureReport::runtime(
                            "sandbox-hash-output",
                            error.to_string(),
                        ))
                    })?;
                let manifest_jsonl = String::from_utf8(manifest_bytes).map_err(|error| {
                    runner_error(SandboxRunnerFailureReport::runtime(
                        "sandbox-manifest-output",
                        error.to_string(),
                    ))
                })?;
                Some(SandboxRunnerLegacyOutput {
                    object_hash: object_hash.to_string(),
                    manifest_jsonl,
                })
            }
            RunnerOutputMode::StepsOnly => None,
        };
        Ok(SandboxRunnerOutcome {
            legacy,
            steps: reports,
        })
    }

    fn prepare(&self) -> RunnerResult<()> {
        for path in &self.prepare_paths {
            fs::create_dir_all(path).map_err(|error| prepare_error("create dir", path, error))?;
            chown_tree(path, BUILD_USER_UID, BUILD_USER_GID)
                .map_err(|error| prepare_error("chown", path, error))?;
        }
        Ok(())
    }

    fn write_success(&self, outcome: &SandboxRunnerOutcome) -> io::Result<()> {
        let report = SandboxRunnerSuccessReport {
            legacy: outcome.legacy.clone(),
            steps: outcome.steps.clone(),
        };
        serde_json::to_writer(&*self.success_report, &report).map_err(io::Error::other)?;
        Ok(())
    }

    fn write_failure(&self, report: &SandboxRunnerFailureReport) -> io::Result<()> {
        serde_json::to_writer(&*self.failure_report, report).map_err(io::Error::other)
    }
}

struct SandboxRunnerOutcome {
    legacy: Option<SandboxRunnerLegacyOutput>,
    steps: Vec<SandboxStepReport>,
}

#[derive(Clone)]
struct SandboxRunnerStep {
    name: String,
    run_as: RunnerRunAs,
    cwd: PathBuf,
    argv: Vec<String>,
    env: HashMap<String, String>,
    umask: u32,
    report_stdout_path: PathBuf,
    report_stderr_path: PathBuf,
    stdout: Arc<File>,
    stderr: Arc<File>,
}

impl SandboxRunnerStep {
    fn new(step: RunnerStepConfig) -> io::Result<Self> {
        validate_umask(step.umask)?;
        Ok(Self {
            name: step.name,
            run_as: step.run_as,
            cwd: step.cwd,
            argv: step.argv,
            env: step.env,
            umask: step.umask,
            report_stdout_path: step.report_stdout_path,
            report_stderr_path: step.report_stderr_path,
            stdout: Arc::new(File::create(&step.stdout_path)?),
            stderr: Arc::new(File::create(&step.stderr_path)?),
        })
    }

    fn run(&self) -> RunnerResult<SandboxStepReport> {
        let executable = self.argv.first().ok_or_else(|| {
            runner_error(SandboxRunnerFailureReport::step_runtime(
                self,
                "step argument vector must contain at least one element".to_string(),
                0,
            ))
        })?;
        let start = Instant::now();
        let stdout = self.stdout.try_clone().map_err(|error| {
            runner_error(SandboxRunnerFailureReport::step_runtime(
                self,
                error.to_string(),
                elapsed_ms(start),
            ))
        })?;
        let stderr = self.stderr.try_clone().map_err(|error| {
            runner_error(SandboxRunnerFailureReport::step_runtime(
                self,
                error.to_string(),
                elapsed_ms(start),
            ))
        })?;

        let mut command = Command::new(executable);
        command
            .args(&self.argv[1..])
            .current_dir(&self.cwd)
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        let run_as = self.run_as;
        let setgroups_allowed = setgroups_allowed();
        let umask = self.umask;
        unsafe {
            command.pre_exec(move || {
                libc::umask(umask as libc::mode_t);
                apply_step_credentials(run_as, setgroups_allowed)
            });
        }

        let mut child = command.spawn().map_err(|error| {
            runner_error(SandboxRunnerFailureReport::step_runtime(
                self,
                format!(
                    "failed to spawn '{}' as {}: {error}",
                    executable,
                    self.run_as.as_str()
                ),
                elapsed_ms(start),
            ))
        })?;
        let child_pid = child.id();

        let status = child.wait().map_err(|error| {
            runner_error(SandboxRunnerFailureReport::step_runtime(
                self,
                format!(
                    "failed to wait for '{}' as {} child pid {}: {error}",
                    executable,
                    self.run_as.as_str(),
                    child_pid
                ),
                elapsed_ms(start),
            ))
        })?;
        terminate_remaining_children();
        let duration_ms = elapsed_ms(start);
        if status.success() {
            Ok(SandboxStepReport {
                name: self.name.clone(),
                run_as: self.run_as.as_str().to_string(),
                exit_code: status.code().unwrap_or(0),
                duration_ms,
                stdout_path: self.report_stdout_path.clone(),
                stderr_path: self.report_stderr_path.clone(),
            })
        } else {
            Err(runner_error(SandboxRunnerFailureReport::failed_step(
                self,
                &status,
                duration_ms,
            )))
        }
    }
}

fn scan_output_manifest(root: &Path) -> RunnerResult<FsTreeManifest> {
    let mut entries = Vec::new();
    scan_output_entry(root, "", &mut entries)?;
    FsTreeManifest::from_entries(entries).map_err(|error| {
        runner_error(SandboxRunnerFailureReport::runtime(
            "sandbox-manifest-output",
            error.to_string(),
        ))
    })
}

fn scan_output_entry(
    path: &Path,
    rel_path: &str,
    entries: &mut Vec<FsTreeEntry>,
) -> RunnerResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        runner_error(SandboxRunnerFailureReport::runtime(
            "sandbox-manifest-output",
            format!("failed to inspect '{}': {error}", path.display()),
        ))
    })?;
    let file_type = metadata.file_type();
    let uid = metadata.uid();
    let gid = metadata.gid();

    if file_type.is_dir() {
        entries.push(FsTreeEntry::directory(
            rel_path,
            uid,
            gid,
            metadata.permissions().mode() & 0o7777,
        ));
        let children = fs::read_dir(path).map_err(|error| {
            runner_error(SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("failed to read directory '{}': {error}", path.display()),
            ))
        })?;
        for child in children {
            let child = child.map_err(|error| {
                runner_error(SandboxRunnerFailureReport::runtime(
                    "sandbox-manifest-output",
                    format!(
                        "failed to read directory entry in '{}': {error}",
                        path.display()
                    ),
                ))
            })?;
            let name = child.file_name();
            let name = name.to_str().ok_or_else(|| {
                runner_error(SandboxRunnerFailureReport::runtime(
                    "sandbox-manifest-output",
                    format!("output path under '{}' is not UTF-8", path.display()),
                ))
            })?;
            let child_rel_path = if rel_path.is_empty() {
                name.to_string()
            } else {
                format!("{rel_path}/{name}")
            };
            scan_output_entry(&child.path(), &child_rel_path, entries)?;
        }
    } else if file_type.is_file() {
        let hash = hash_path(path).map_err(|error| {
            runner_error(SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("failed to hash output file '{}': {error}", path.display()),
            ))
        })?;
        entries.push(FsTreeEntry::file_with_hash(
            rel_path,
            uid,
            gid,
            metadata.permissions().mode() & 0o7777,
            hash,
        ));
    } else if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|error| {
            runner_error(SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("failed to read symlink '{}': {error}", path.display()),
            ))
        })?;
        let target = target.to_str().ok_or_else(|| {
            runner_error(SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("symlink target for '{}' is not UTF-8", path.display()),
            ))
        })?;
        entries.push(FsTreeEntry::symlink_with_hash(
            rel_path,
            uid,
            gid,
            target,
            hash_symlink_node(target.as_bytes()),
        ));
    } else {
        return Err(runner_error(SandboxRunnerFailureReport::runtime(
            "sandbox-manifest-output",
            format!("unsupported output file type '{}'", path.display()),
        )));
    }

    Ok(())
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

fn prepare_error(
    operation: &str,
    path: &Path,
    error: io::Error,
) -> Box<SandboxRunnerFailureReport> {
    runner_error(SandboxRunnerFailureReport::runtime(
        "sandbox-prepare",
        format!("{operation} '{}': {error}", path.display()),
    ))
}

fn terminate_remaining_children() {
    #[cfg(test)]
    {
        // Unit tests run multiple runner instances concurrently in one process.
        // A process-wide waitpid(-1) here can reap another test's step child
        // before that test calls Child::wait, turning a step failure report
        // into a runtime wait error.
    }
    #[cfg(not(test))]
    {
        unsafe {
            libc::kill(-1, libc::SIGTERM);
        }
        reap_finished_children();
        unsafe {
            libc::kill(-1, libc::SIGKILL);
        }
        reap_finished_children();
    }
}

#[cfg(not(test))]
fn reap_finished_children() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(_) => continue,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(_) => break,
        }
    }
}

fn validate_umask(umask: u32) -> io::Result<()> {
    if umask <= 0o777 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("sandbox step umask must be in 0o000..=0o777, got {umask:#o}"),
        ))
    }
}

fn setgroups_allowed() -> bool {
    match fs::read_to_string("/proc/self/setgroups") {
        Ok(value) => value.trim() != "deny",
        Err(_) => true,
    }
}

fn apply_step_credentials(run_as: RunnerRunAs, setgroups_allowed: bool) -> io::Result<()> {
    match run_as {
        RunnerRunAs::BuildUser => {
            if setgroups_allowed && let Err(error) = setgroups(&[]) {
                return Err(credential_error("setgroups([])", error));
            }
            if let Err(error) = setgid(Gid::from_raw(BUILD_USER_GID)) {
                return Err(credential_error("setgid(1)", error));
            }
            if let Err(error) = setuid(Uid::from_raw(BUILD_USER_UID)) {
                return Err(credential_error("setuid(1)", error));
            }
            Ok(())
        }
        RunnerRunAs::Root => Ok(()),
    }
}

fn credential_error(operation: &str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{operation}: {error}"))
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        lchown(path, uid, gid)?;
        return Ok(());
    }

    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(io::Error::other)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let result = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

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
            PathBuf::from("__mbuild/out")
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
            protocol_version: RUNNER_PROTOCOL_VERSION,
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
    fn protocol_info_reports_current_runner_protocol() {
        let info = protocol_info();

        assert_eq!(info.name, RUNNER_BINARY_NAME);
        assert_eq!(info.protocol_version, RUNNER_PROTOCOL_VERSION);
    }

    #[test]
    fn run_config_rejects_protocol_mismatch_with_failure_report() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path()).with_protocol(RUNNER_PROTOCOL_VERSION + 1);

        let exit_code = run_config(config.clone());

        assert_eq!(exit_code, 1);
        let report = read_failure_report(&config.failure_report);
        assert_eq!(report.label, "runner-protocol");
        assert!(
            report.message.contains("unsupported runner protocol"),
            "{}",
            report.message
        );
    }

    #[test]
    fn run_config_executes_root_step_and_writes_success_report() {
        let temp = tempdir().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let config = test_config(temp.path())
            .with_output(output.clone())
            .with_step(
                "write-output",
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf payload > \"$OUT/file\"; printf hello".to_string(),
                ],
                HashMap::from([("OUT".to_string(), output.display().to_string())]),
                stdout.clone(),
                stderr.clone(),
            );

        let exit_code = run_config(config.clone());

        assert_eq!(exit_code, 0);
        assert_eq!(fs::read_to_string(output.join("file")).unwrap(), "payload");
        assert_eq!(fs::read_to_string(stdout).unwrap(), "hello");
        let report = read_success_report(&config.success_report);
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "write-output");
        let legacy = report.legacy.expect("legacy output report");
        let manifest =
            FsTreeManifest::parse_canonical_bytes(legacy.manifest_jsonl.as_bytes()).unwrap();
        assert!(
            manifest
                .entries()
                .iter()
                .any(|entry| matches!(entry, FsTreeEntry::File { path, .. } if path == "file"))
        );
    }

    #[test]
    fn run_config_steps_only_success_report_skips_legacy_output_scan() {
        let temp = tempdir().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let fifo = output.join("fifo");
        let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o644) };
        assert_eq!(result, 0, "mkfifo failed: {}", io::Error::last_os_error());
        let config = test_config(temp.path())
            .with_output(output)
            .with_output_mode(RunnerOutputMode::StepsOnly)
            .with_step(
                "no-output-scan",
                vec!["/bin/sh".to_string(), "-c".to_string(), ":".to_string()],
                HashMap::new(),
                stdout,
                stderr,
            );

        let exit_code = run_config(config.clone());

        if exit_code != 0 {
            panic!("{:?}", read_failure_report(&config.failure_report));
        }
        let report = read_success_report(&config.success_report);
        assert!(report.legacy.is_none());
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "no-output-scan");
    }

    #[test]
    fn run_config_writes_failure_report_for_failed_step() {
        let temp = tempdir().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let config = test_config(temp.path()).with_output(output).with_step(
            "fail-step",
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf nope >&2; exit 7".to_string(),
            ],
            HashMap::new(),
            stdout.clone(),
            stderr.clone(),
        );

        let exit_code = run_config(config.clone());

        assert_eq!(exit_code, 1);
        assert_eq!(fs::read_to_string(&stderr).unwrap(), "nope");
        let report = read_failure_report(&config.failure_report);
        assert_eq!(report.label, "fail-step");
        assert_eq!(report.exit_code, Some(7));
        assert_eq!(report.stdout_path.as_ref(), Some(&stdout));
        assert_eq!(report.stderr_path.as_ref(), Some(&stderr));
    }

    #[test]
    fn run_config_applies_step_umask() {
        let temp = tempdir().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let config = test_config(temp.path())
            .with_output(output.clone())
            .with_step_umask(
                "write-mode",
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    ": > \"$OUT/file\"".to_string(),
                ],
                HashMap::from([("OUT".to_string(), output.display().to_string())]),
                0o077,
                stdout,
                stderr,
            );

        let exit_code = run_config(config);

        assert_eq!(exit_code, 0);
        let mode = fs::metadata(output.join("file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn run_config_rejects_invalid_step_umask() {
        let temp = tempdir().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let config = test_config(temp.path())
            .with_output(output)
            .with_step_umask(
                "bad-umask",
                vec!["true".to_string()],
                HashMap::new(),
                0o1000,
                stdout,
                stderr,
            );

        let exit_code = run_config(config);

        assert_eq!(exit_code, 2);
    }

    #[test]
    fn run_config_path_rejects_malformed_json() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("runner-config.json");
        fs::write(&path, "not json").unwrap();

        let exit_code = run_config_path(&path);

        assert_eq!(exit_code, 2);
    }

    #[test]
    fn scan_output_manifest_records_kinds_modes_owners_and_symlink_targets() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("out");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("file"), "contents").unwrap();
        fs::write(root.join("exe"), "contents").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(root.join("dir"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o640)).unwrap();
        fs::set_permissions(root.join("exe"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("file", root.join("link")).unwrap();
        let owner = fs::symlink_metadata(&root).unwrap();

        let manifest = scan_output_manifest(&root).unwrap();

        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "",
            owner.uid(),
            owner.gid(),
            0o755
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "dir",
            owner.uid(),
            owner.gid(),
            0o700
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::file_with_hash(
            "file",
            owner.uid(),
            owner.gid(),
            0o640,
            hash_path(root.join("file")).unwrap()
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::file_with_hash(
            "exe",
            owner.uid(),
            owner.gid(),
            0o755,
            hash_path(root.join("exe")).unwrap()
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::symlink_with_hash(
            "link",
            owner.uid(),
            owner.gid(),
            "file",
            hash_symlink_node(b"file")
        )));
    }

    #[derive(Clone)]
    struct TestConfig {
        config: RunnerConfig,
    }

    impl TestConfig {
        fn with_protocol(mut self, protocol_version: u32) -> RunnerConfig {
            self.config.protocol_version = protocol_version;
            self.config
        }

        fn with_output(mut self, output_dir: PathBuf) -> Self {
            self.config.output_dir = output_dir;
            self
        }

        fn with_output_mode(mut self, output_mode: RunnerOutputMode) -> Self {
            self.config.output_mode = output_mode;
            self
        }

        fn with_step(
            mut self,
            name: &str,
            argv: Vec<String>,
            env: HashMap<String, String>,
            stdout_path: PathBuf,
            stderr_path: PathBuf,
        ) -> RunnerConfig {
            self.push_step(name, argv, env, 0o022, stdout_path, stderr_path);
            self.config
        }

        fn with_step_umask(
            mut self,
            name: &str,
            argv: Vec<String>,
            env: HashMap<String, String>,
            umask: u32,
            stdout_path: PathBuf,
            stderr_path: PathBuf,
        ) -> RunnerConfig {
            self.push_step(name, argv, env, umask, stdout_path, stderr_path);
            self.config
        }

        fn push_step(
            &mut self,
            name: &str,
            argv: Vec<String>,
            env: HashMap<String, String>,
            umask: u32,
            stdout_path: PathBuf,
            stderr_path: PathBuf,
        ) {
            self.config.steps.push(RunnerStepConfig {
                name: name.to_string(),
                run_as: RunnerRunAs::Root,
                cwd: PathBuf::from("/"),
                argv,
                env,
                umask,
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
                report_stdout_path: stdout_path,
                report_stderr_path: stderr_path,
            });
        }
    }

    fn test_config(root: &Path) -> TestConfig {
        TestConfig {
            config: RunnerConfig {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                prepare_paths: Vec::new(),
                steps: Vec::new(),
                output_dir: root.join("out"),
                output_mode: RunnerOutputMode::LegacyFsTreeReport,
                success_report: root.join("success.json"),
                failure_report: root.join("failure.json"),
            },
        }
    }

    fn read_success_report(path: &Path) -> SandboxRunnerSuccessReport {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn read_failure_report(path: &Path) -> SandboxRunnerFailureReport {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }
}
