//! Runtime-backed sandbox build execution.

use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use fsobj_hash::ObjectHash;
use mbuild_core::FsTreeManifest;
use mbuild_sandbox_runner_core::{
    RUNNER_BINARY_NAME, RUNNER_PROTOCOL_VERSION, RunnerConfig, RunnerProtocolInfo, RunnerRunAs,
    RunnerStepConfig, SandboxLauncherConfig, SandboxLauncherMount, SandboxLauncherMountKind,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use tracing::warn;
use uuid::Uuid;

const CONTAINER_RUNTIME_DIR: &str = "/__mbuild/runtime";
const CONTAINER_RUNNER_DIR: &str = "/__mbuild/runner";
const CONTAINER_LOG_DIR: &str = "/__mbuild/logs";
const CONTAINER_RUNNER_CONFIG: &str = "/__mbuild/runtime/runner-config.json";
const CONTAINER_SUCCESS_REPORT: &str = "/__mbuild/runtime/sandbox-success.json";
const CONTAINER_FAILURE_REPORT: &str = "/__mbuild/runtime/sandbox-failure.json";
const CONTAINER_BREADCRUMBS: &str = "/__mbuild/runtime/sandbox-breadcrumbs.log";

/// User identity used for a sandbox step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRunAs {
    /// Run as the sandbox build user, currently numeric `1:1`.
    BuildUser,
    /// Run as container root, currently numeric `0:0`.
    Root,
}

/// A named input mount passed to a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxInput {
    /// Recipe input name.
    pub name: String,
    /// Host path to the realized input object.
    pub host_path: PathBuf,
    /// Absolute mount path inside the sandbox.
    pub mount_path: PathBuf,
}

/// One command step to execute inside a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxStep {
    /// Step name.
    pub name: String,
    /// User identity for the step.
    pub run_as: SandboxRunAs,
    /// Absolute working directory inside the sandbox.
    pub cwd: PathBuf,
    /// Argument vector to execute.
    pub argv: Vec<String>,
    /// Environment variables for the step.
    pub env: HashMap<String, String>,
    /// Host log path for stdout.
    pub stdout_path: PathBuf,
    /// Host log path for stderr.
    pub stderr_path: PathBuf,
}

/// Runtime configuration for one sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxBuildConfig {
    /// Realized rootfs object path.
    pub rootfs: PathBuf,
    /// Host directory for build output.
    pub out_dir: PathBuf,
    /// Host directory for script config.
    pub config_dir: PathBuf,
    /// Host workspace for temporary runtime state.
    pub workspace: PathBuf,
    /// Additional named inputs.
    pub inputs: Vec<SandboxInput>,
    /// Ordered build steps.
    pub steps: Vec<SandboxStep>,
}

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

/// Structured report for one sandbox step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxStepReport {
    /// Step name.
    pub name: String,
    /// Step user identity.
    pub run_as: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Step duration in milliseconds.
    pub duration_ms: u128,
    /// Host stdout log path.
    pub stdout_path: PathBuf,
    /// Host stderr log path.
    pub stderr_path: PathBuf,
}

/// Execute a complete sandbox build and return the output hash.
pub fn run_sandbox_build(
    config: SandboxBuildConfig,
    idmap: &MbuildIdmap,
) -> Result<SandboxBuildOutcome, RuntimeError> {
    validate_config(&config)?;
    crate::preflight::preflight_local_helper_runtime(idmap)?;
    let tools = cached_sandbox_tools()?;

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

fn validate_config(config: &SandboxBuildConfig) -> Result<(), RuntimeError> {
    require_directory(&config.rootfs, "sandbox rootfs")?;
    require_directory(&config.out_dir, "sandbox output directory")?;
    require_directory(&config.config_dir, "sandbox config directory")?;
    require_directory(&config.workspace, "sandbox workspace")?;
    reject_reserved_rootfs_entry(&config.rootfs, "__mbuild")?;
    for input in &config.inputs {
        if input.mount_path.is_relative() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox input '{}' mount path must be absolute: '{}'",
                input.name,
                input.mount_path.display()
            )));
        }
        if !input.host_path.is_dir() && !input.host_path.is_file() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox input '{}' must resolve to a file or directory: '{}'",
                input.name,
                input.host_path.display()
            )));
        }
    }
    Ok(())
}

struct PreparedSandbox {
    dirs: SandboxDirs,
    runtime_files: SandboxRuntimeFiles,
    launcher_config: PathBuf,
}

impl PreparedSandbox {
    fn create(config: &SandboxBuildConfig, runner_path: &Path) -> Result<Self, RuntimeError> {
        let dirs = SandboxDirs::create(&config.workspace)?;
        let runtime_files = SandboxRuntimeFiles::create(&dirs.runtime_files, config)?;
        write_runner_config(config, &runtime_files)?;
        populate_root_skeleton(&dirs.rootfs, &config.rootfs, &runtime_files)?;
        let launcher_config = dirs.root.join("launcher-config.json");
        let launcher = build_launcher_config(config, &dirs, &runtime_files, runner_path)?;
        serde_json::to_writer(File::create(&launcher_config)?, &launcher)
            .map_err(|error| RuntimeError::Executor(error.to_string()))?;
        Ok(Self {
            dirs,
            runtime_files,
            launcher_config,
        })
    }
}

struct SandboxDirs {
    root: PathBuf,
    rootfs: PathBuf,
    build_dir: PathBuf,
    runtime_files: PathBuf,
}

impl SandboxDirs {
    fn create(workspace: &Path) -> Result<Self, RuntimeError> {
        let root = workspace
            .join("sandbox")
            .join(Uuid::new_v4().simple().to_string());
        let rootfs = root.join("rootfs");
        let build_dir = root.join("build");
        let runtime_files = root.join("runtime-files");

        fs::create_dir_all(&rootfs)?;
        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&runtime_files)?;

        Ok(Self {
            root,
            rootfs,
            build_dir,
            runtime_files,
        })
    }
}

fn write_runner_config(
    config: &SandboxBuildConfig,
    runtime_files: &SandboxRuntimeFiles,
) -> Result<(), RuntimeError> {
    let steps = config
        .steps
        .iter()
        .zip(runtime_files.step_logs.iter())
        .map(|(step, logs)| RunnerStepConfig {
            name: step.name.clone(),
            run_as: match step.run_as {
                SandboxRunAs::BuildUser => RunnerRunAs::BuildUser,
                SandboxRunAs::Root => RunnerRunAs::Root,
            },
            cwd: step.cwd.clone(),
            argv: step.argv.clone(),
            env: step_env(step),
            stdout_path: logs.container_stdout.clone(),
            stderr_path: logs.container_stderr.clone(),
            report_stdout_path: step.stdout_path.clone(),
            report_stderr_path: step.stderr_path.clone(),
        })
        .collect::<Vec<_>>();
    let runner_config = RunnerConfig {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        prepare_paths: vec![PathBuf::from("/__mbuild/build")],
        steps,
        output_dir: PathBuf::from("/__mbuild/out"),
        success_report: PathBuf::from(CONTAINER_SUCCESS_REPORT),
        failure_report: PathBuf::from(CONTAINER_FAILURE_REPORT),
        breadcrumbs: PathBuf::from(CONTAINER_BREADCRUMBS),
    };
    serde_json::to_writer(File::create(&runtime_files.runner_config)?, &runner_config)
        .map_err(|error| RuntimeError::Executor(error.to_string()))
}

struct SandboxRuntimeFiles {
    root: PathBuf,
    success_report: PathBuf,
    failure_report: PathBuf,
    runner_config: PathBuf,
    step_logs: Vec<SandboxStepLogMounts>,
}

impl SandboxRuntimeFiles {
    fn create(root: &Path, config: &SandboxBuildConfig) -> Result<Self, RuntimeError> {
        fs::create_dir_all(root)?;
        let success_report = root.join("sandbox-success.json");
        let failure_report = root.join("sandbox-failure.json");
        let breadcrumbs = root.join("sandbox-breadcrumbs.log");
        let runner_config = root.join("runner-config.json");
        File::create(&success_report)?;
        File::create(&failure_report)?;
        File::create(&breadcrumbs)?;
        File::create(&runner_config)?;
        fs::create_dir_all(root.join("logs"))?;
        let mut step_logs = Vec::new();
        for (index, step) in config.steps.iter().enumerate() {
            File::create(&step.stdout_path)?;
            File::create(&step.stderr_path)?;
            step_logs.push(SandboxStepLogMounts {
                host_stdout: step.stdout_path.clone(),
                host_stderr: step.stderr_path.clone(),
                container_stdout: Path::new(CONTAINER_LOG_DIR).join(format!("{index}.stdout")),
                container_stderr: Path::new(CONTAINER_LOG_DIR).join(format!("{index}.stderr")),
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
            success_report,
            failure_report,
            runner_config,
            step_logs,
        })
    }
}

struct SandboxStepLogMounts {
    host_stdout: PathBuf,
    host_stderr: PathBuf,
    container_stdout: PathBuf,
    container_stderr: PathBuf,
}

#[derive(Debug)]
struct SandboxTools {
    runner: SandboxRunnerBinary,
    newuidmap: PathBuf,
    newgidmap: PathBuf,
}

#[derive(Debug)]
struct SandboxRunnerBinary {
    host_path: PathBuf,
}

fn cached_sandbox_tools() -> Result<Arc<SandboxTools>, RuntimeError> {
    static TOOLS: OnceLock<Result<Arc<SandboxTools>, String>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            resolve_and_preflight_sandbox_tools()
                .map(Arc::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|message| RuntimeError::Preflight(message.clone()))
}

fn resolve_and_preflight_sandbox_tools() -> Result<SandboxTools, RuntimeError> {
    Ok(SandboxTools {
        runner: resolve_and_preflight_sandbox_runner()?,
        newuidmap: resolve_path_program(OsStr::new("newuidmap"))?,
        newgidmap: resolve_path_program(OsStr::new("newgidmap"))?,
    })
}

fn resolve_and_preflight_sandbox_runner() -> Result<SandboxRunnerBinary, RuntimeError> {
    let host_path = resolve_sandbox_runner_path()?;
    require_executable_file(&host_path, "sandbox runner")?;
    require_static_elf(&host_path)?;
    let output = Command::new(&host_path)
        .arg("--protocol-info")
        .output()
        .map_err(|error| {
            RuntimeError::Preflight(format!(
                "failed to run sandbox runner preflight '{} --protocol-info': {error}",
                host_path.display()
            ))
        })?;
    if !output.status.success() {
        return Err(RuntimeError::Preflight(format!(
            "sandbox runner preflight '{} --protocol-info' failed with status {}: {}",
            host_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let info = serde_json::from_slice::<RunnerProtocolInfo>(&output.stdout).map_err(|error| {
        RuntimeError::Preflight(format!(
            "failed to parse sandbox runner protocol info from '{}': {error}",
            host_path.display()
        ))
    })?;
    if info.name != RUNNER_BINARY_NAME || info.protocol_version != RUNNER_PROTOCOL_VERSION {
        return Err(RuntimeError::Preflight(format!(
            "sandbox runner '{}' has incompatible protocol {:?}; expected name '{}' protocol {}",
            host_path.display(),
            info,
            RUNNER_BINARY_NAME,
            RUNNER_PROTOCOL_VERSION
        )));
    }
    Ok(SandboxRunnerBinary { host_path })
}

fn resolve_sandbox_runner_path() -> Result<PathBuf, RuntimeError> {
    resolve_sandbox_runner_path_from(
        env::var_os("MBUILD_SANDBOX_RUNNER").map(PathBuf::from),
        env::current_exe().ok().as_deref(),
        env::var_os("PATH"),
    )
}

fn resolve_sandbox_runner_path_from(
    env_override: Option<PathBuf>,
    current_exe: Option<&Path>,
    path_env: Option<OsString>,
) -> Result<PathBuf, RuntimeError> {
    let mut checked = Vec::new();
    if let Some(path) = env_override {
        checked.push(path.clone());
        if path.exists() {
            return Ok(path);
        }
    }

    if let Some(current_exe) = current_exe {
        if let Some((target_dir, profile)) = cargo_target_dir_and_profile(current_exe) {
            let candidate = target_dir
                .join("x86_64-unknown-linux-musl")
                .join(profile)
                .join(RUNNER_BINARY_NAME);
            checked.push(candidate.clone());
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join(RUNNER_BINARY_NAME);
            checked.push(sibling.clone());
            if sibling.exists() {
                return Ok(sibling);
            }
        }
        for ancestor in current_exe.ancestors() {
            let target_dir = ancestor.join("target");
            for profile in ["debug", "release"] {
                let candidate = target_dir
                    .join("x86_64-unknown-linux-musl")
                    .join(profile)
                    .join(RUNNER_BINARY_NAME);
                checked.push(candidate.clone());
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    if let Some(path) = path_env {
        for dir in env::split_paths(&path) {
            let candidate = dir.join(RUNNER_BINARY_NAME);
            checked.push(candidate.clone());
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Err(RuntimeError::Preflight(format!(
        "failed to find sandbox runner '{}'; checked {}",
        RUNNER_BINARY_NAME,
        checked
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

fn cargo_target_dir_and_profile(current_exe: &Path) -> Option<(&Path, &str)> {
    let profile_dir = current_exe.parent()?;
    let profile = profile_dir.file_name()?.to_str()?;
    if !matches!(profile, "debug" | "release") {
        return None;
    }
    let parent = profile_dir.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("target") {
        return Some((parent, profile));
    }
    let target_dir = parent.parent()?;
    if target_dir.file_name().and_then(|name| name.to_str()) == Some("target") {
        return Some((target_dir, profile));
    }
    None
}

fn resolve_path_program(name: &OsStr) -> Result<PathBuf, RuntimeError> {
    let Some(path_env) = env::var_os("PATH") else {
        return Err(RuntimeError::Preflight(format!(
            "{} not found: PATH is unset",
            name.to_string_lossy()
        )));
    };
    for dir in env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.exists() {
            require_executable_file(&candidate, &name.to_string_lossy())?;
            return Ok(candidate);
        }
    }
    Err(RuntimeError::Preflight(format!(
        "{} not found in PATH",
        name.to_string_lossy()
    )))
}

fn require_executable_file(path: &Path, label: &str) -> Result<(), RuntimeError> {
    let metadata = fs::metadata(path).map_err(|error| {
        RuntimeError::Preflight(format!(
            "{label} '{}' cannot be inspected: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(RuntimeError::Preflight(format!(
            "{label} '{}' is not a file",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(RuntimeError::Preflight(format!(
            "{label} '{}' is not executable",
            path.display()
        )));
    }
    Ok(())
}

fn require_static_elf(path: &Path) -> Result<(), RuntimeError> {
    let bytes = fs::read(path).map_err(|error| {
        RuntimeError::Preflight(format!(
            "failed to read sandbox runner '{}': {error}",
            path.display()
        ))
    })?;
    match elf_has_interpreter(&bytes) {
        Ok(true) => Err(RuntimeError::Preflight(format!(
            "sandbox runner '{}' is dynamically linked; build a static musl runner",
            path.display()
        ))),
        Ok(false) => Ok(()),
        Err(message) => Err(RuntimeError::Preflight(format!(
            "failed to inspect sandbox runner ELF '{}': {message}",
            path.display()
        ))),
    }
}

fn elf_has_interpreter(bytes: &[u8]) -> Result<bool, String> {
    const PT_INTERP: u32 = 3;
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        return Err("not an ELF file".to_string());
    }
    if bytes[4] != 2 {
        return Err("unsupported non-64-bit ELF".to_string());
    }
    if bytes[5] != 1 {
        return Err("unsupported non-little-endian ELF".to_string());
    }
    let phoff = read_u64_le(bytes, 32)? as usize;
    let phentsize = read_u16_le(bytes, 54)? as usize;
    let phnum = read_u16_le(bytes, 56)? as usize;
    if phentsize < 4 {
        return Err("invalid ELF program header size".to_string());
    }
    for index in 0..phnum {
        let offset = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or("ELF program headers overflow")?,
            )
            .ok_or("ELF program headers overflow")?;
        let end = offset
            .checked_add(phentsize)
            .ok_or("ELF program header overflow")?;
        if end > bytes.len() {
            return Err("ELF program header outside file".to_string());
        }
        if read_u32_le(bytes, offset)? == PT_INTERP {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or("unexpected end of ELF file")?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or("unexpected end of ELF file")?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or("unexpected end of ELF file")?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

struct SandboxLifecycle {
    child: Option<Child>,
    run_dir: PathBuf,
    success_report_path: PathBuf,
    failure_report_path: PathBuf,
    cleaned_up: bool,
}

impl SandboxLifecycle {
    fn start(
        tools: &SandboxTools,
        idmap: &MbuildIdmap,
        prepared: PreparedSandbox,
    ) -> Result<Self, RuntimeError> {
        let child_ready = Pipe::new()?;
        let parent_ready = Pipe::new()?;
        let child_ready_read = child_ready.read_raw();
        let child_ready_write = child_ready.write_raw();
        let parent_ready_read = parent_ready.read_raw();
        let parent_ready_write = parent_ready.write_raw();

        let mut command = Command::new(&tools.runner.host_path);
        command
            .arg("launch")
            .arg("--wait-fd")
            .arg(parent_ready_read.to_string())
            .arg("--config")
            .arg(&prepared.launcher_config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        unsafe {
            command.pre_exec(move || {
                if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let byte = [1_u8; 1];
                let written = libc::write(child_ready_write, byte.as_ptr().cast(), byte.len());
                if written != 1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(|error| {
            RuntimeError::Executor(format!(
                "failed to spawn sandbox runner '{}': {error}",
                tools.runner.host_path.display()
            ))
        })?;
        let Pipe {
            read: child_ready_read_fd,
            write: child_ready_write_fd,
        } = child_ready;
        let Pipe {
            read: parent_ready_read_fd,
            write: parent_ready_write_fd,
        } = parent_ready;
        drop(child_ready_write_fd);
        drop(parent_ready_read_fd);

        let setup_result = wait_for_child_userns(child_ready_read)
            .and_then(|()| configure_id_maps(&tools.newuidmap, &tools.newgidmap, child.id(), idmap))
            .and_then(|()| signal_child_ready(parent_ready_write));

        drop(parent_ready_write_fd);
        drop(child_ready_read_fd);

        if let Err(error) = setup_result {
            terminate_child(&mut child);
            return Err(error);
        }

        Ok(Self {
            child: Some(child),
            run_dir: prepared.dirs.root,
            success_report_path: prepared.runtime_files.success_report,
            failure_report_path: prepared.runtime_files.failure_report,
            cleaned_up: false,
        })
    }

    fn wait_for_outcome(&mut self) -> Result<SandboxBuildOutcome, RuntimeError> {
        let child = self
            .child
            .take()
            .ok_or_else(|| RuntimeError::Executor("sandbox runner already waited".to_string()))?;
        let output = child.wait_with_output()?;
        if output.status.success() {
            read_sandbox_success_report(&self.success_report_path)
        } else {
            Err(read_sandbox_failure_report(
                &self.failure_report_path,
                format!(
                    "sandbox runner exited with {}{}",
                    status_message(output.status),
                    command_context(&output.stderr)
                ),
            ))
        }
    }

    fn cleanup(&mut self) -> Result<(), RuntimeError> {
        if self.cleaned_up {
            return Ok(());
        }
        if let Some(child) = &mut self.child {
            terminate_child(child);
        }
        remove_run_dir(&self.run_dir)?;
        self.cleaned_up = true;
        Ok(())
    }
}

impl Drop for SandboxLifecycle {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        if let Some(child) = &mut self.child {
            terminate_child(child);
        }
        if let Err(error) = remove_run_dir(&self.run_dir) {
            warn!(
                "failed to remove sandbox workspace '{}' during drop: {error}",
                self.run_dir.display()
            );
        }
    }
}

fn wait_for_child_userns(fd: RawFd) -> Result<(), RuntimeError> {
    read_one_byte(fd, "child user namespace setup")
}

fn signal_child_ready(fd: RawFd) -> Result<(), RuntimeError> {
    let byte = [1_u8; 1];
    let written = unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) };
    if written == 1 {
        Ok(())
    } else {
        Err(RuntimeError::Executor(format!(
            "failed to signal sandbox runner readiness: {}",
            io::Error::last_os_error()
        )))
    }
}

fn read_one_byte(fd: RawFd, label: &str) -> Result<(), RuntimeError> {
    let mut byte = [0_u8; 1];
    loop {
        let result = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if result == 1 {
            return Ok(());
        }
        if result == 0 {
            return Err(RuntimeError::Executor(format!(
                "sandbox runner closed {label} pipe before signalling readiness"
            )));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(RuntimeError::Executor(format!(
                "failed to read sandbox runner {label} pipe: {error}"
            )));
        }
    }
}

fn configure_id_maps(
    newuidmap: &Path,
    newgidmap: &Path,
    pid: u32,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    run_map_command(
        newuidmap,
        pid,
        [
            ("0", idmap.current_uid(), 1),
            ("1", idmap.subuid_base(), idmap.subuid_count()),
        ],
    )?;
    write_setgroups_deny(pid)?;
    run_map_command(
        newgidmap,
        pid,
        [
            ("0", idmap.current_gid(), 1),
            ("1", idmap.subgid_base(), idmap.subgid_count()),
        ],
    )
}

fn run_map_command<const N: usize>(
    program: &Path,
    pid: u32,
    ranges: [(&str, u32, u32); N],
) -> Result<(), RuntimeError> {
    let mut command = Command::new(program);
    command.arg(pid.to_string());
    for (inside, outside, count) in ranges {
        command
            .arg(inside)
            .arg(outside.to_string())
            .arg(count.to_string());
    }
    let output = command.output().map_err(|error| {
        RuntimeError::Preflight(format!("failed to run '{}': {error}", program.display()))
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(RuntimeError::Executor(format!(
            "'{}' failed with {}{}",
            program.display(),
            status_message(output.status),
            command_context(&output.stderr)
        )))
    }
}

fn write_setgroups_deny(pid: u32) -> Result<(), RuntimeError> {
    let path = PathBuf::from(format!("/proc/{pid}/setgroups"));
    match fs::write(&path, b"deny\n") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::Executor(format!(
            "failed to write '{}': {error}",
            path.display()
        ))),
    }
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn remove_run_dir(path: &Path) -> Result<(), RuntimeError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::Io(error)),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SandboxRunnerSuccessReport {
    object_hash: String,
    manifest_jsonl: String,
    steps: Vec<SandboxStepReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SandboxRunnerFailureReport {
    label: String,
    message: String,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: Option<u128>,
    stdout_path: Option<PathBuf>,
    stderr_path: Option<PathBuf>,
}

impl SandboxRunnerFailureReport {
    fn to_error_message(&self) -> String {
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

fn read_sandbox_success_report(path: &Path) -> Result<SandboxBuildOutcome, RuntimeError> {
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

fn read_sandbox_failure_report(path: &Path, fallback: String) -> RuntimeError {
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

fn build_launcher_config(
    config: &SandboxBuildConfig,
    dirs: &SandboxDirs,
    runtime_files: &SandboxRuntimeFiles,
    runner_path: &Path,
) -> Result<SandboxLauncherConfig, RuntimeError> {
    let mut mounts = rootfs_top_level_mounts(&config.rootfs)?;
    mounts.extend([
        proc_mount(Path::new("/proc")),
        tmpfs_mount(Path::new("/tmp"), &["mode=1777"]),
        tmpfs_mount(Path::new("/run"), &["mode=755"]),
        bind_mount(&dirs.build_dir, Path::new("/__mbuild/build"), false),
        bind_mount(&config.config_dir, Path::new("/__mbuild/config"), true),
        bind_mount(&config.out_dir, Path::new("/__mbuild/out"), false),
        bind_mount(
            runner_path,
            &Path::new(CONTAINER_RUNNER_DIR).join(RUNNER_BINARY_NAME),
            true,
        ),
        bind_mount(&runtime_files.root, Path::new(CONTAINER_RUNTIME_DIR), false),
    ]);

    for log in &runtime_files.step_logs {
        mounts.push(bind_mount(&log.host_stdout, &log.container_stdout, false));
        mounts.push(bind_mount(&log.host_stderr, &log.container_stderr, false));
    }

    for input in &config.inputs {
        mounts.push(bind_mount(&input.host_path, &input.mount_path, true));
    }

    Ok(SandboxLauncherConfig {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        root: dirs.rootfs.clone(),
        mounts,
        runner_config: PathBuf::from(CONTAINER_RUNNER_CONFIG),
        failure_report: runtime_files.failure_report.clone(),
    })
}

fn rootfs_top_level_mounts(rootfs: &Path) -> Result<Vec<SandboxLauncherMount>, RuntimeError> {
    let mut entries = rootfs_top_level_entries(rootfs)?;
    entries.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    let mut mounts = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "sandbox rootfs '{}' contains non-UTF-8 top-level entry",
                rootfs.display()
            ))
        })?;
        let source = entry.path();
        let destination = Path::new("/").join(name);
        let file_type = entry.file_type()?;

        if !should_mount_rootfs_entry(name) {
            continue;
        }

        if file_type.is_dir() || file_type.is_file() {
            mounts.push(bind_mount(&source, &destination, true));
        }
    }

    Ok(mounts)
}

fn rootfs_top_level_entries(rootfs: &Path) -> Result<Vec<fs::DirEntry>, RuntimeError> {
    fs::read_dir(rootfs)?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(RuntimeError::from)
}

fn should_mount_rootfs_entry(name: &str) -> bool {
    !matches!(name, "__mbuild" | "dev" | "proc" | "run" | "tmp")
}

fn populate_root_skeleton(
    sandbox_root: &Path,
    lower_rootfs: &Path,
    runtime_files: &SandboxRuntimeFiles,
) -> Result<(), RuntimeError> {
    for entry in rootfs_top_level_entries(lower_rootfs)? {
        let name = entry.file_name();
        let destination = sandbox_root.join(&name);
        let file_type = entry.file_type()?;

        if file_type.is_symlink() {
            if let Some(name) = name.to_str()
                && should_mount_rootfs_entry(name)
            {
                let target = fs::read_link(entry.path())?;
                if !destination.exists() && !destination.is_symlink() {
                    symlink(target, destination)?;
                }
            }
        } else if file_type.is_dir() {
            let name = name.to_str().ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "sandbox rootfs '{}' contains non-UTF-8 top-level entry",
                    lower_rootfs.display()
                ))
            })?;
            if !should_mount_rootfs_entry(name) {
                fs::create_dir_all(destination)?;
            }
        }
    }
    for path in [
        Path::new("__mbuild"),
        Path::new("__mbuild/build"),
        Path::new("__mbuild/config"),
        Path::new("__mbuild/inputs"),
        Path::new("__mbuild/logs"),
        Path::new("__mbuild/out"),
        Path::new("__mbuild/runner"),
        Path::new("__mbuild/runtime"),
        Path::new("proc"),
        Path::new("run"),
        Path::new("tmp"),
    ] {
        fs::create_dir_all(sandbox_root.join(path))?;
    }
    File::create(
        sandbox_root
            .join("__mbuild/runner")
            .join(RUNNER_BINARY_NAME),
    )?;
    for log in &runtime_files.step_logs {
        create_mount_target(sandbox_root, &log.container_stdout)?;
        create_mount_target(sandbox_root, &log.container_stderr)?;
    }
    Ok(())
}

fn create_mount_target(sandbox_root: &Path, container_path: &Path) -> Result<(), RuntimeError> {
    let relative = container_path.strip_prefix("/").map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "container mount target '{}' must be absolute",
            container_path.display()
        ))
    })?;
    let target = sandbox_root.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    File::create(target)?;
    Ok(())
}

fn bind_mount(source: &Path, target: &Path, readonly: bool) -> SandboxLauncherMount {
    SandboxLauncherMount {
        kind: SandboxLauncherMountKind::Bind,
        source: Some(source.to_path_buf()),
        target: target.to_path_buf(),
        readonly,
        options: Vec::new(),
    }
}

fn proc_mount(target: &Path) -> SandboxLauncherMount {
    SandboxLauncherMount {
        kind: SandboxLauncherMountKind::Proc,
        source: None,
        target: target.to_path_buf(),
        readonly: false,
        options: Vec::new(),
    }
}

fn tmpfs_mount(target: &Path, extra_options: &[&str]) -> SandboxLauncherMount {
    let mut options = vec![
        "nosuid".to_string(),
        "nodev".to_string(),
        "noexec".to_string(),
    ];
    options.extend(extra_options.iter().map(|option| option.to_string()));
    SandboxLauncherMount {
        kind: SandboxLauncherMountKind::Tmpfs,
        source: None,
        target: target.to_path_buf(),
        readonly: false,
        options,
    }
}

fn step_env(step: &SandboxStep) -> HashMap<String, String> {
    let mut env = HashMap::from([
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        ("HOME".to_string(), "/__mbuild/build".to_string()),
        ("TMPDIR".to_string(), "/tmp".to_string()),
        ("USER".to_string(), "mbuild".to_string()),
        (
            "MBUILD_CONFIG_DIR".to_string(),
            "/__mbuild/config".to_string(),
        ),
        (
            "MBUILD_BUILD_DIR".to_string(),
            "/__mbuild/build".to_string(),
        ),
        ("MBUILD_OUT_DIR".to_string(), "/__mbuild/out".to_string()),
        ("MBUILD_STEP_NAME".to_string(), step.name.clone()),
    ]);
    env.extend(step.env.clone());
    env
}

fn require_directory(path: &Path, label: &str) -> Result<(), RuntimeError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "{label} '{}' must exist and be a directory",
            path.display()
        )))
    }
}

fn reject_reserved_rootfs_entry(rootfs: &Path, name: &str) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(rootfs.join(name)) {
        Ok(_) => Err(RuntimeError::InvalidInput(format!(
            "sandbox rootfs '{}' contains reserved top-level entry '/{name}'",
            rootfs.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::Io(error)),
    }
}

fn status_message(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => match status.signal() {
            Some(signal) => format!("signal {signal}"),
            None => "unknown status".to_string(),
        },
    }
}

fn command_context(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| format!(": {line}"))
        .unwrap_or_default()
}

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

    fn read_raw(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    fn write_raw(&self) -> RawFd {
        self.write.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsobj_hash::hash_fs_tree_object;
    use mbuild_core::FsTreeEntry;
    use tempfile::tempdir;

    #[test]
    fn launcher_config_uses_readonly_rootfs_binds_and_writable_runtime_mounts() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let source = temp.path().join("source");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        for path in [&rootfs, &source, &out, &config, &workspace] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(source.join("main.c"), "int main(void) { return 0; }\n").unwrap();
        for name in ["dev", "etc", "proc", "run", "tmp", "usr", "var"] {
            fs::create_dir(rootfs.join(name)).unwrap();
        }
        symlink("usr/bin", rootfs.join("bin")).unwrap();
        let build_config = SandboxBuildConfig {
            rootfs: rootfs.clone(),
            out_dir: out.clone(),
            config_dir: config,
            workspace: workspace.clone(),
            inputs: vec![SandboxInput {
                name: "source".to_string(),
                host_path: source.clone(),
                mount_path: PathBuf::from("/__mbuild/inputs/source"),
            }],
            steps: Vec::new(),
        };
        let dirs = SandboxDirs::create(&workspace).unwrap();
        let runtime_files =
            SandboxRuntimeFiles::create(&dirs.runtime_files, &build_config).unwrap();
        let runner_path = temp.path().join(RUNNER_BINARY_NAME);
        fs::write(&runner_path, "#!/bin/sh\n").unwrap();

        let launcher =
            build_launcher_config(&build_config, &dirs, &runtime_files, &runner_path).unwrap();
        let mounts = &launcher.mounts;

        assert_eq!(launcher.protocol_version, RUNNER_PROTOCOL_VERSION);
        assert_eq!(launcher.root, dirs.rootfs);
        for name in ["usr", "etc", "var"] {
            let destination = Path::new("/").join(name);
            let mount = mounts
                .iter()
                .find(|mount| mount.target == destination)
                .unwrap_or_else(|| panic!("/{name} readonly bind mount exists"));
            assert_eq!(mount.kind, SandboxLauncherMountKind::Bind);
            assert_eq!(mount.source.as_deref(), Some(rootfs.join(name).as_path()));
            assert!(mount.readonly);
        }
        assert!(!mounts.iter().any(|mount| mount.target == Path::new("/dev")
            && mount.source.as_deref() == Some(rootfs.join("dev").as_path())));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/__mbuild/build")
                && mount.source.as_deref() == Some(dirs.build_dir.as_path())
                && !mount.readonly
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/__mbuild/out")
                && mount.source.as_deref() == Some(out.as_path())
                && !mount.readonly
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/tmp") && mount.kind == SandboxLauncherMountKind::Tmpfs
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/run") && mount.kind == SandboxLauncherMountKind::Tmpfs
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/__mbuild/config")
                && mount.source.as_deref() == Some(build_config.config_dir.as_path())
                && mount.readonly
        }));
        let source_bind = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/__mbuild/inputs/source"))
            .expect("source input bind mount exists");
        assert_eq!(source_bind.source.as_deref(), Some(source.as_path()));
        assert!(source_bind.readonly);
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.target == Path::new("/etc/hosts"))
        );
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.target == Path::new("/etc/resolv.conf"))
        );
        assert!(mounts.iter().any(|mount| {
            mount.target
                == Path::new(CONTAINER_RUNNER_DIR)
                    .join(RUNNER_BINARY_NAME)
                    .as_path()
                && mount.source.as_deref() == Some(runner_path.as_path())
                && mount.readonly
        }));
        assert!(mounts.iter().all(|mount| {
            mount
                .options
                .iter()
                .all(|option| !option.contains("cgroup"))
        }));
    }

    #[test]
    fn sandbox_config_rejects_reserved_mbuild_rootfs_entry() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        for path in [&rootfs, &out, &config, &workspace] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir(rootfs.join("__mbuild")).unwrap();
        let build_config = SandboxBuildConfig {
            rootfs: rootfs.clone(),
            out_dir: out,
            config_dir: config,
            workspace,
            inputs: Vec::new(),
            steps: Vec::new(),
        };

        let error = validate_config(&build_config).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("reserved top-level entry"));
    }

    #[test]
    fn sandbox_root_skeleton_copies_top_level_symlinks_and_skipped_dirs() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let root = temp.path().join("root");
        fs::create_dir_all(&lower).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(lower.join("run")).unwrap();
        fs::create_dir(lower.join("usr")).unwrap();
        symlink("usr/bin", lower.join("bin")).unwrap();

        let config = SandboxBuildConfig {
            rootfs: lower.clone(),
            out_dir: temp.path().join("out"),
            config_dir: temp.path().join("config"),
            workspace: temp.path().join("workspace"),
            inputs: Vec::new(),
            steps: Vec::new(),
        };
        for path in [&config.out_dir, &config.config_dir, &config.workspace] {
            fs::create_dir_all(path).unwrap();
        }
        let runtime_files =
            SandboxRuntimeFiles::create(&config.workspace.join("runtime-files"), &config).unwrap();

        populate_root_skeleton(&root, &lower, &runtime_files).unwrap();

        assert_eq!(
            fs::read_link(root.join("bin")).unwrap(),
            Path::new("usr/bin")
        );
        assert!(root.join("run").is_dir());
        assert!(!root.join("usr").exists());
    }

    #[test]
    fn sandbox_runner_resolution_prefers_musl_runner_in_cargo_dev_tree() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let debug = target.join("debug");
        let musl_debug = target.join("x86_64-unknown-linux-musl").join("debug");
        fs::create_dir_all(&debug).unwrap();
        fs::create_dir_all(&musl_debug).unwrap();
        let current_exe = debug.join("mbuild");
        let dynamic_sibling = debug.join(RUNNER_BINARY_NAME);
        let static_runner = musl_debug.join(RUNNER_BINARY_NAME);
        fs::write(&current_exe, "").unwrap();
        fs::write(&dynamic_sibling, "").unwrap();
        fs::write(&static_runner, "").unwrap();

        let resolved = resolve_sandbox_runner_path_from(None, Some(&current_exe), None).unwrap();

        assert_eq!(resolved, static_runner);
    }

    #[test]
    fn sandbox_runner_resolution_uses_installed_sibling_outside_cargo_tree() {
        let temp = tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let current_exe = bin.join("mbuild");
        let sibling = bin.join(RUNNER_BINARY_NAME);
        fs::write(&current_exe, "").unwrap();
        fs::write(&sibling, "").unwrap();

        let resolved = resolve_sandbox_runner_path_from(None, Some(&current_exe), None).unwrap();

        assert_eq!(resolved, sibling);
    }

    #[test]
    fn elf_interpreter_detection_finds_dynamic_runner_shape() {
        assert!(elf_has_interpreter(&minimal_elf64_with_program_header(3)).unwrap());
        assert!(!elf_has_interpreter(&minimal_elf64_with_program_header(1)).unwrap());
    }

    fn minimal_elf64_with_program_header(program_type: u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; 64 + 56];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[32..40].copy_from_slice(&(64_u64).to_le_bytes());
        bytes[54..56].copy_from_slice(&(56_u16).to_le_bytes());
        bytes[56..58].copy_from_slice(&(1_u16).to_le_bytes());
        bytes[64..68].copy_from_slice(&program_type.to_le_bytes());
        bytes
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
