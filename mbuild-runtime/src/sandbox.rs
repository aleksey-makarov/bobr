//! Runtime-backed sandbox build execution.

use crate::bundle::{Bundle, create_bundle};
use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use crate::preflight::preflight_ownership_runtime;
use fsobj_hash::ObjectHash;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::oci_spec::runtime::{
    Capabilities, Capability, LinuxBuilder, LinuxCapabilities, LinuxCapabilitiesBuilder,
    LinuxIdMapping, LinuxIdMappingBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, Mount,
    MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};
use libcontainer::syscall::syscall::SyscallType;
use libcontainer::workload::{
    Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError,
};
use mbuild_core::FsTreeManifest;
use mbuild_sandbox_runner_core::{
    RUNNER_BINARY_NAME, RUNNER_PROTOCOL_VERSION, RunnerConfig, RunnerProtocolInfo, RunnerRunAs,
    RunnerStepConfig,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Pid, execvp};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::ffi::{CString, OsString};
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tracing::warn;
use uuid::Uuid;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const CONTAINER_RUNTIME_DIR: &str = "/__mbuild/runtime";
const CONTAINER_RUNNER_DIR: &str = "/__mbuild/runner";
const CONTAINER_LOG_DIR: &str = "/__mbuild/logs";
const CONTAINER_RUNNER_CONFIG: &str = "/__mbuild/runtime/runner-config.json";
const CONTAINER_SUCCESS_REPORT: &str = "/__mbuild/runtime/sandbox-success.json";
const CONTAINER_FAILURE_REPORT: &str = "/__mbuild/runtime/sandbox-failure.json";
const CONTAINER_BREADCRUMBS: &str = "/__mbuild/runtime/sandbox-breadcrumbs.log";
const ROOT_STEP_CAPABILITIES: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
];
const RUNNER_EXTRA_CAPABILITIES: &[&str] = &["CAP_SETGID", "CAP_SETPCAP", "CAP_SETUID"];

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
    /// Persistent root for libcontainer state.
    pub state_dir: PathBuf,
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
    preflight_ownership_runtime(idmap)?;
    let runner = cached_sandbox_runner()?;

    let prepared = PreparedSandbox::create(&config, idmap, &runner.host_path)?;
    write_runner_config(&config, &prepared.runtime_files)?;
    let mut lifecycle = SandboxLifecycle::start(
        prepared.bundle,
        prepared.runtime_files,
        prepared.host_cgroup_path,
        &config.state_dir,
    )?;
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

fn libcontainer_start_lock() -> Result<MutexGuard<'static, ()>, RuntimeError> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // libcontainer 0.6 creates/connects notify.sock by temporarily changing the
    // process cwd, so concurrent container build/start calls are unsafe.
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| RuntimeError::Libcontainer("libcontainer start lock is poisoned".to_string()))
}

fn validate_config(config: &SandboxBuildConfig) -> Result<(), RuntimeError> {
    require_directory(&config.rootfs, "sandbox rootfs")?;
    require_directory(&config.out_dir, "sandbox output directory")?;
    require_directory(&config.config_dir, "sandbox config directory")?;
    require_directory(&config.workspace, "sandbox workspace")?;
    require_directory(&config.state_dir, "sandbox state directory")?;
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
    bundle: Bundle,
    runtime_files: SandboxRuntimeFiles,
    /// Real `/sys/fs/cgroup/...` directory we created and own; must be
    /// cleaned up after the sandbox shuts down.
    host_cgroup_path: PathBuf,
}

impl PreparedSandbox {
    fn create(
        config: &SandboxBuildConfig,
        idmap: &MbuildIdmap,
        runner_path: &Path,
    ) -> Result<Self, RuntimeError> {
        let dirs = SandboxDirs::create(&config.workspace)?;
        let runtime_files = SandboxRuntimeFiles::create(&dirs.runtime_files, config)?;
        let cgroup = prepare_sandbox_cgroup()?;
        let spec = build_sandbox_spec(
            config,
            idmap,
            &dirs,
            &runtime_files,
            runner_path,
            Some(cgroup.container_path),
        )?;
        let bundle = create_bundle(&config.workspace, &spec)?;
        populate_bundle_rootfs_skeleton(bundle.rootfs_dir(), &config.rootfs, &runtime_files)?;
        Ok(Self {
            bundle,
            runtime_files,
            host_cgroup_path: cgroup.host_path,
        })
    }
}

struct SandboxDirs {
    build_dir: PathBuf,
    runtime_files: PathBuf,
}

impl SandboxDirs {
    fn create(workspace: &Path) -> Result<Self, RuntimeError> {
        let root = workspace
            .join("sandbox")
            .join(Uuid::new_v4().simple().to_string());
        let build_dir = root.join("build");
        let runtime_files = root.join("runtime-files");

        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&runtime_files)?;

        Ok(Self {
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
        let logs = root.join("logs");
        fs::create_dir_all(&logs)?;
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

struct SandboxRunnerBinary {
    host_path: PathBuf,
}

fn cached_sandbox_runner() -> Result<Arc<SandboxRunnerBinary>, RuntimeError> {
    static RUNNER: OnceLock<Result<Arc<SandboxRunnerBinary>, String>> = OnceLock::new();
    RUNNER
        .get_or_init(|| {
            resolve_and_preflight_sandbox_runner()
                .map(Arc::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|message| RuntimeError::Preflight(message.clone()))
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
    container: libcontainer::container::Container,
    init_pid: Pid,
    _bundle: Bundle,
    success_report_path: PathBuf,
    failure_report_path: PathBuf,
    /// Real `/sys/fs/cgroup/...` directory created by prepare_sandbox_cgroup;
    /// removed by Drop (and explicit cleanup) to avoid orphan cgroup leaves
    /// when the sandbox finishes — gracefully or via panic/unwind.
    host_cgroup_path: PathBuf,
    /// True once `cleanup()` has run successfully, so Drop does not redo the
    /// kill/delete/rmdir dance and does not log spurious warnings.
    cleaned_up: bool,
}

impl SandboxLifecycle {
    fn start(
        bundle: Bundle,
        runtime_files: SandboxRuntimeFiles,
        host_cgroup_path: PathBuf,
        state_dir: &Path,
    ) -> Result<Self, RuntimeError> {
        let container_id = format!("mbuild-sandbox-{}", Uuid::new_v4().simple());
        // .with_systemd(false) is largely advisory: libcontainer's
        // create_cgroup_manager (libcgroups/src/common.rs) routes through the
        // cgroupfs v2 manager whenever spec.linux.cgroups_path is absolute,
        // bypassing systemd DBus regardless of this flag. We rely on
        // prepare_sandbox_cgroup() supplying that absolute path inside a
        // user-delegated slice. If a future change drops the absolute path,
        // libcontainer would fall back to the systemd cgroup manager.
        let _start_guard = libcontainer_start_lock()?;
        let mut container = ContainerBuilder::new(container_id.clone(), SyscallType::Linux)
            .with_executor(SandboxInitExec)
            .with_root_path(state_dir)
            .map_err(libcontainer_error)?
            .as_init(bundle.dir())
            .with_systemd(false)
            .with_detach(false)
            .build()
            .map_err(libcontainer_error)?;
        let init_pid = container.pid().ok_or_else(|| {
            RuntimeError::Libcontainer("libcontainer did not expose sandbox init pid".to_string())
        })?;
        container.start().map_err(libcontainer_error)?;
        drop(_start_guard);

        Ok(Self {
            container,
            init_pid,
            _bundle: bundle,
            success_report_path: runtime_files.success_report,
            failure_report_path: runtime_files.failure_report,
            host_cgroup_path,
            cleaned_up: false,
        })
    }

    fn wait_for_outcome(&self) -> Result<SandboxBuildOutcome, RuntimeError> {
        match waitpid(self.init_pid, None).map_err(libcontainer_error)? {
            WaitStatus::Exited(_, 0) => read_sandbox_success_report(&self.success_report_path),
            WaitStatus::Exited(_, code) => Err(read_sandbox_failure_report(
                &self.failure_report_path,
                format!("sandbox runner exited with status {code}"),
            )),
            WaitStatus::Signaled(_, signal, _) => Err(read_sandbox_failure_report(
                &self.failure_report_path,
                format!("sandbox runner was killed by signal {signal}"),
            )),
            status => Err(RuntimeError::Executor(format!(
                "sandbox runner ended with wait status {status:?}"
            ))),
        }
    }

    fn cleanup(&mut self) -> Result<(), RuntimeError> {
        if self.cleaned_up {
            return Ok(());
        }
        // delete(true) sends SIGKILL if the container is still running, then
        // removes libcontainer's state directory and the cgroup it created
        // for us (when cgroupfs manager is in use, that is the directory we
        // pre-created).
        let result = self.container.delete(true).map_err(libcontainer_error);
        // Best-effort rmdir of the cgroup we own. After successful delete it
        // is normally already gone; the call here is a safety net for builds
        // that placed the container in a non-trivial cgroup state.
        if let Err(error) = fs::remove_dir(&self.host_cgroup_path)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(
                "failed to remove sandbox cgroup '{}': {error}",
                self.host_cgroup_path.display()
            );
        }
        self.cleaned_up = true;
        result
    }
}

impl Drop for SandboxLifecycle {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        // Best-effort cleanup for unwind/panic paths where cleanup() was
        // never called explicitly. Without this, the runner init process and
        // its children could outlive mbuild and leave a stuck cgroup when the
        // user systemd session is degraded.
        if let Err(error) = self.container.kill(nix::sys::signal::SIGKILL, true) {
            warn!("failed to SIGKILL sandbox container during drop: {error}");
        }
        if let Err(error) = self.container.delete(true) {
            warn!("failed to delete sandbox container during drop: {error}");
        }
        if let Err(error) = fs::remove_dir(&self.host_cgroup_path)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(
                "failed to remove sandbox cgroup '{}' during drop: {error}",
                self.host_cgroup_path.display()
            );
        }
    }
}

#[derive(Clone)]
struct SandboxInitExec;

impl Executor for SandboxInitExec {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, spec: &Spec) -> Result<(), ExecutorValidationError> {
        let args = spec
            .process()
            .as_ref()
            .and_then(|process| process.args().as_ref())
            .ok_or_else(|| {
                ExecutorValidationError::ArgValidationError(
                    "sandbox runner process args are missing".to_string(),
                )
            })?;
        if args.is_empty() {
            return Err(ExecutorValidationError::ArgValidationError(
                "sandbox runner process args are empty".to_string(),
            ));
        }
        Ok(())
    }

    fn exec(&self, spec: &Spec) -> Result<(), ExecutorError> {
        let args = spec
            .process()
            .as_ref()
            .and_then(|process| process.args().as_ref())
            .ok_or(ExecutorError::InvalidArg)?;
        let executable = args.first().ok_or(ExecutorError::InvalidArg)?;
        let c_executable = CString::new(executable.as_bytes()).map_err(executor_error)?;
        let c_args = args
            .iter()
            .map(|arg| CString::new(arg.as_bytes()).map_err(executor_error))
            .collect::<Result<Vec<_>, _>>()?;
        execvp(&c_executable, &c_args).map_err(|error| {
            ExecutorError::Execution(
                format!("failed to exec sandbox runner '{executable}': {error}").into(),
            )
        })?;
        unreachable!();
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

fn build_sandbox_spec(
    config: &SandboxBuildConfig,
    idmap: &MbuildIdmap,
    dirs: &SandboxDirs,
    runtime_files: &SandboxRuntimeFiles,
    runner_path: &Path,
    cgroup_path: Option<PathBuf>,
) -> Result<Spec, RuntimeError> {
    let uid_mappings = vec![
        linux_id_mapping(0, idmap.current_uid(), 1)?,
        linux_id_mapping(1, idmap.subuid_base(), idmap.subuid_count())?,
    ];
    let gid_mappings = vec![
        linux_id_mapping(0, idmap.current_gid(), 1)?,
        linux_id_mapping(1, idmap.subgid_base(), idmap.subgid_count())?,
    ];

    let mut linux = build_oci(
        LinuxBuilder::default()
            // Cgroup namespace is unshared so the sandbox init-runner sees a
            // container-local cgroup view while libcontainer still places the
            // process in the per-sandbox absolute cgroup path prepared below.
            .namespaces(vec![
                namespace(LinuxNamespaceType::User)?,
                namespace(LinuxNamespaceType::Mount)?,
                namespace(LinuxNamespaceType::Pid)?,
                namespace(LinuxNamespaceType::Uts)?,
                namespace(LinuxNamespaceType::Ipc)?,
                namespace(LinuxNamespaceType::Network)?,
                namespace(LinuxNamespaceType::Cgroup)?,
            ])
            .uid_mappings(uid_mappings)
            .gid_mappings(gid_mappings)
            .masked_paths(Vec::<String>::new())
            .readonly_paths(Vec::<String>::new())
            .build(),
    )?;
    linux.set_resources(None);
    if let Some(cgroup_path) = cgroup_path {
        linux.set_cgroups_path(Some(cgroup_path));
    }

    let mut mounts = rootfs_top_level_mounts(&config.rootfs)?;
    mounts.extend([
        proc_mount()?,
        tmpfs_mount(Path::new("/tmp"), &["mode=1777"])?,
        tmpfs_mount(Path::new("/run"), &["mode=755"])?,
        bind_mount(&dirs.build_dir, Path::new("/__mbuild/build"), false)?,
        bind_mount(&config.config_dir, Path::new("/__mbuild/config"), true)?,
        bind_mount(&config.out_dir, Path::new("/__mbuild/out"), false)?,
        bind_mount(
            runner_path,
            &Path::new(CONTAINER_RUNNER_DIR).join(RUNNER_BINARY_NAME),
            true,
        )?,
        bind_mount(&runtime_files.root, Path::new(CONTAINER_RUNTIME_DIR), false)?,
    ]);

    for log in &runtime_files.step_logs {
        mounts.push(bind_mount(&log.host_stdout, &log.container_stdout, false)?);
        mounts.push(bind_mount(&log.host_stderr, &log.container_stderr, false)?);
    }

    for input in &config.inputs {
        mounts.push(bind_mount(&input.host_path, &input.mount_path, true)?);
    }

    build_oci(
        SpecBuilder::default()
            .version("1.0.2")
            .hostname("mbuild")
            .root(build_oci(
                RootBuilder::default().path("rootfs").readonly(true).build(),
            )?)
            .process(build_oci(
                ProcessBuilder::default()
                    .terminal(false)
                    .user(build_oci(
                        UserBuilder::default().uid(0_u32).gid(0_u32).build(),
                    )?)
                    .args(vec![
                        Path::new(CONTAINER_RUNNER_DIR)
                            .join(RUNNER_BINARY_NAME)
                            .display()
                            .to_string(),
                        CONTAINER_RUNNER_CONFIG.to_string(),
                    ])
                    .cwd("/")
                    .capabilities(runner_linux_capabilities()?)
                    .no_new_privileges(true)
                    .build(),
            )?)
            .mounts(mounts)
            .linux(linux)
            .build(),
    )
}

fn rootfs_top_level_mounts(rootfs: &Path) -> Result<Vec<Mount>, RuntimeError> {
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

        if file_type.is_dir() {
            mounts.push(bind_mount(&source, &destination, true)?);
        } else if file_type.is_file() {
            mounts.push(bind_mount(&source, &destination, true)?);
        }
    }

    Ok(mounts)
}

/// Host-side and container-side paths of a freshly created sandbox cgroup.
struct SandboxCgroup {
    /// Real `/sys/fs/cgroup/...` path; used to rmdir at cleanup time.
    host_path: PathBuf,
    /// Absolute path within the cgroup hierarchy used in `linux.cgroups_path`
    /// of the OCI spec.
    container_path: PathBuf,
}

// Pre-create a cgroup directory inside the user-delegated slice so that the
// container ends up with an absolute cgroup path. libcontainer's
// create_cgroup_manager (libcgroups/src/common.rs) uses the cgroupfs v2
// manager whenever the path is absolute, bypassing the systemd DBus path that
// would otherwise be forced by user namespaces (and which can deadlock if the
// user systemd session is unhealthy).
fn prepare_sandbox_cgroup() -> Result<SandboxCgroup, RuntimeError> {
    let current = current_cgroup_v2_path()?;
    let name = format!("mbuild-sandbox-{}", Uuid::new_v4().simple());
    let relative = current.join(name);
    let host_path = Path::new(CGROUP_ROOT).join(&relative);
    fs::create_dir(&host_path).map_err(|error| {
        RuntimeError::Executor(format!(
            "failed to create sandbox cgroup '{}': {error}",
            host_path.display()
        ))
    })?;
    let container_path = Path::new("/").join(relative);
    Ok(SandboxCgroup {
        host_path,
        container_path,
    })
}

fn current_cgroup_v2_path() -> Result<PathBuf, RuntimeError> {
    let cgroup = fs::read_to_string("/proc/self/cgroup")?;
    for line in cgroup.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Ok(path
                .strip_prefix('/')
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(path)));
        }
    }
    Err(RuntimeError::Executor(
        "failed to find current cgroup v2 path in /proc/self/cgroup".to_string(),
    ))
}

fn rootfs_top_level_entries(rootfs: &Path) -> Result<Vec<fs::DirEntry>, RuntimeError> {
    fs::read_dir(rootfs)?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(RuntimeError::from)
}

fn should_mount_rootfs_entry(name: &str) -> bool {
    !matches!(name, "__mbuild" | "dev" | "proc" | "run" | "tmp")
}

fn populate_bundle_rootfs_skeleton(
    bundle_rootfs: &Path,
    lower_rootfs: &Path,
    runtime_files: &SandboxRuntimeFiles,
) -> Result<(), RuntimeError> {
    for entry in rootfs_top_level_entries(lower_rootfs)? {
        let name = entry.file_name();
        let destination = bundle_rootfs.join(&name);
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
        fs::create_dir_all(bundle_rootfs.join(path))?;
    }
    File::create(
        bundle_rootfs
            .join("__mbuild/runner")
            .join(RUNNER_BINARY_NAME),
    )?;
    for log in &runtime_files.step_logs {
        create_bundle_mount_target(bundle_rootfs, &log.container_stdout)?;
        create_bundle_mount_target(bundle_rootfs, &log.container_stderr)?;
    }
    Ok(())
}

fn create_bundle_mount_target(
    bundle_rootfs: &Path,
    container_path: &Path,
) -> Result<(), RuntimeError> {
    let relative = container_path.strip_prefix("/").map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "container mount target '{}' must be absolute",
            container_path.display()
        ))
    })?;
    let target = bundle_rootfs.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    File::create(target)?;
    Ok(())
}

fn bind_mount(source: &Path, destination: &Path, readonly: bool) -> Result<Mount, RuntimeError> {
    let mut options = vec!["rbind".to_string()];
    if readonly {
        options.push("ro".to_string());
    } else {
        options.push("rw".to_string());
    }
    build_oci(
        MountBuilder::default()
            .destination(destination)
            .typ("bind")
            .source(source)
            .options(options)
            .build(),
    )
}

fn proc_mount() -> Result<Mount, RuntimeError> {
    build_oci(
        MountBuilder::default()
            .destination(Path::new("/proc"))
            .typ("proc")
            .source(Path::new("proc"))
            .build(),
    )
}

fn tmpfs_mount(destination: &Path, extra_options: &[&str]) -> Result<Mount, RuntimeError> {
    let mut options = vec![
        "nosuid".to_string(),
        "nodev".to_string(),
        "noexec".to_string(),
    ];
    options.extend(extra_options.iter().map(|option| option.to_string()));
    build_oci(
        MountBuilder::default()
            .destination(destination)
            .typ("tmpfs")
            .source(Path::new("tmpfs"))
            .options(options)
            .build(),
    )
}

fn namespace(
    typ: LinuxNamespaceType,
) -> Result<libcontainer::oci_spec::runtime::LinuxNamespace, RuntimeError> {
    build_oci(LinuxNamespaceBuilder::default().typ(typ).build())
}

fn linux_id_mapping(
    container_id: u32,
    host_id: u32,
    size: u32,
) -> Result<LinuxIdMapping, RuntimeError> {
    build_oci(
        LinuxIdMappingBuilder::default()
            .container_id(container_id)
            .host_id(host_id)
            .size(size)
            .build(),
    )
}

fn runner_linux_capabilities() -> Result<LinuxCapabilities, RuntimeError> {
    let caps = runner_capability_names()
        .into_iter()
        .map(oci_capability_from_name)
        .collect::<Result<Capabilities, _>>()?;

    build_oci(
        LinuxCapabilitiesBuilder::default()
            .bounding(caps.clone())
            .effective(caps.clone())
            .inheritable(caps.clone())
            .permitted(caps.clone())
            .ambient(caps)
            .build(),
    )
}

fn runner_capability_names() -> Vec<&'static str> {
    ROOT_STEP_CAPABILITIES
        .iter()
        .chain(RUNNER_EXTRA_CAPABILITIES.iter())
        .copied()
        .collect()
}

fn oci_capability_from_name(name: &str) -> Result<Capability, RuntimeError> {
    match name {
        "CAP_CHOWN" => Ok(Capability::Chown),
        "CAP_DAC_OVERRIDE" => Ok(Capability::DacOverride),
        "CAP_DAC_READ_SEARCH" => Ok(Capability::DacReadSearch),
        "CAP_FOWNER" => Ok(Capability::Fowner),
        "CAP_FSETID" => Ok(Capability::Fsetid),
        "CAP_SETGID" => Ok(Capability::Setgid),
        "CAP_SETPCAP" => Ok(Capability::Setpcap),
        "CAP_SETUID" => Ok(Capability::Setuid),
        _ => Err(RuntimeError::InvalidInput(format!(
            "unsupported sandbox capability '{name}'"
        ))),
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

fn build_oci<T>(result: Result<T, impl std::fmt::Display>) -> Result<T, RuntimeError> {
    result.map_err(|error| RuntimeError::Libcontainer(error.to_string()))
}

fn libcontainer_error(error: impl Error) -> RuntimeError {
    RuntimeError::Libcontainer(format_error_chain(&error))
}

fn format_error_chain(error: &dyn Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

fn executor_error(error: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsobj_hash::hash_fs_tree_object;
    use mbuild_core::FsTreeEntry;
    use tempfile::tempdir;

    #[test]
    fn sandbox_spec_uses_readonly_rootfs_binds_and_writable_runtime_mounts() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let source = temp.path().join("source");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        for path in [&rootfs, &source, &out, &config, &workspace, &state] {
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
            state_dir: state,
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
        let idmap = MbuildIdmap::for_tests(1000, 1001, 100000, 65536, 200000, 65536);

        let cgroup_path = PathBuf::from("/user.slice/mbuild-test.scope");
        let spec = build_sandbox_spec(
            &build_config,
            &idmap,
            &dirs,
            &runtime_files,
            &runner_path,
            Some(cgroup_path.clone()),
        )
        .unwrap();
        let mounts = spec.mounts().as_ref().unwrap();
        let linux = spec.linux().as_ref().unwrap();
        let root = spec.root().as_ref().unwrap();

        assert_eq!(linux.cgroups_path().as_ref(), Some(&cgroup_path));
        assert!(linux.resources().is_none());
        assert_eq!(root.readonly(), Some(true));

        assert!(
            !mounts
                .iter()
                .any(|mount| mount.typ().as_deref() == Some("overlay"))
        );
        for name in ["usr", "etc", "var"] {
            let destination = Path::new("/").join(name);
            let mount = mounts
                .iter()
                .find(|mount| {
                    mount.destination() == destination.as_path()
                        && mount.typ().as_deref() == Some("bind")
                })
                .unwrap_or_else(|| panic!("/{name} readonly bind mount exists"));
            assert_eq!(mount.source().as_deref(), Some(rootfs.join(name).as_path()));
            assert!(
                mount
                    .options()
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|option| option == "ro")
            );
        }
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/dev")
                    && mount.source().as_deref() == Some(rootfs.join("dev").as_path()))
        );
        let build_bind = mounts
            .iter()
            .find(|mount| {
                mount.destination() == Path::new("/__mbuild/build")
                    && mount.typ().as_deref() == Some("bind")
            })
            .expect("/__mbuild/build bind mount exists");
        assert_eq!(
            build_bind.source().as_deref(),
            Some(dirs.build_dir.as_path())
        );
        assert!(
            build_bind
                .options()
                .as_ref()
                .unwrap()
                .iter()
                .any(|option| option == "rw")
        );
        assert!(mounts.iter().any(|mount| {
            mount.destination() == Path::new("/__mbuild/out")
                && mount.source().as_deref() == Some(out.as_path())
                && mount.typ().as_deref() == Some("bind")
                && mount
                    .options()
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|option| option == "rw")
        }));
        assert!(
            mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/tmp")
                    && mount.typ().as_deref() == Some("tmpfs"))
        );
        assert!(
            mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/run")
                    && mount.typ().as_deref() == Some("tmpfs"))
        );
        assert!(mounts.iter().any(|mount| {
            mount.destination() == Path::new("/__mbuild/config")
                && mount.source().as_deref() == Some(build_config.config_dir.as_path())
                && mount.typ().as_deref() == Some("bind")
                && mount
                    .options()
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|option| option == "ro")
        }));
        let source_bind = mounts
            .iter()
            .find(|mount| {
                mount.destination() == Path::new("/__mbuild/inputs/source")
                    && mount.typ().as_deref() == Some("bind")
            })
            .expect("source input bind mount exists");
        assert_eq!(source_bind.source().as_deref(), Some(source.as_path()));
        assert!(
            source_bind
                .options()
                .as_ref()
                .unwrap()
                .iter()
                .any(|option| option == "ro")
        );
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/etc/hosts"))
        );
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/etc/resolv.conf"))
        );
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.destination()
                    == Path::new("/__mbuild/runtime/output-hash.json"))
        );
        assert!(mounts.iter().any(|mount| {
            mount.destination()
                == Path::new(CONTAINER_RUNNER_DIR)
                    .join(RUNNER_BINARY_NAME)
                    .as_path()
                && mount.source().as_deref() == Some(runner_path.as_path())
        }));
        let capabilities = spec
            .process()
            .as_ref()
            .unwrap()
            .capabilities()
            .as_ref()
            .unwrap()
            .effective()
            .as_ref()
            .unwrap();
        assert!(capabilities.contains(&Capability::Setuid));
        assert!(capabilities.contains(&Capability::Setgid));
        assert!(capabilities.contains(&Capability::Chown));
    }

    #[test]
    fn sandbox_config_rejects_reserved_mbuild_rootfs_entry() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        for path in [&rootfs, &out, &config, &workspace, &state] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir(rootfs.join("__mbuild")).unwrap();
        let build_config = SandboxBuildConfig {
            rootfs: rootfs.clone(),
            out_dir: out,
            config_dir: config,
            workspace,
            state_dir: state,
            inputs: Vec::new(),
            steps: Vec::new(),
        };

        let error = validate_config(&build_config).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("reserved top-level entry"));
    }

    #[test]
    fn sandbox_bundle_skeleton_copies_top_level_symlinks_and_skipped_dirs() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let bundle = temp.path().join("bundle");
        fs::create_dir_all(&lower).unwrap();
        fs::create_dir_all(&bundle).unwrap();
        fs::create_dir(lower.join("run")).unwrap();
        fs::create_dir(lower.join("usr")).unwrap();
        symlink("usr/bin", lower.join("bin")).unwrap();

        let config = SandboxBuildConfig {
            rootfs: lower.clone(),
            out_dir: temp.path().join("out"),
            config_dir: temp.path().join("config"),
            workspace: temp.path().join("workspace"),
            state_dir: temp.path().join("state"),
            inputs: Vec::new(),
            steps: Vec::new(),
        };
        for path in [
            &config.out_dir,
            &config.config_dir,
            &config.workspace,
            &config.state_dir,
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let runtime_files =
            SandboxRuntimeFiles::create(&config.workspace.join("runtime-files"), &config).unwrap();

        populate_bundle_rootfs_skeleton(&bundle, &lower, &runtime_files).unwrap();

        assert_eq!(
            fs::read_link(bundle.join("bin")).unwrap(),
            Path::new("usr/bin")
        );
        assert!(bundle.join("run").is_dir());
        assert!(!bundle.join("usr").exists());
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
