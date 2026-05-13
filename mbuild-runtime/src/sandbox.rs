//! Runtime-backed sandbox build execution.

use crate::bundle::{Bundle, create_bundle};
use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use crate::preflight::preflight_ownership_runtime;
use fsobj_hash::{ObjectHash, hash_fs_tree_object};
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
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Gid, Pid, Uid, chown, setgid, setgroups, setuid};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;
use uuid::Uuid;

const BUILD_USER_UID: u32 = 1;
const BUILD_USER_GID: u32 = 1;
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
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

    let prepared = PreparedSandbox::create(&config, idmap)?;
    let runner = SandboxRunnerExecutor::new(&config, &prepared.runtime_files)?;
    let mut lifecycle = SandboxLifecycle::start(
        prepared.bundle,
        runner,
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

fn validate_config(config: &SandboxBuildConfig) -> Result<(), RuntimeError> {
    require_directory(&config.rootfs, "sandbox rootfs")?;
    require_directory(&config.out_dir, "sandbox output directory")?;
    require_directory(&config.config_dir, "sandbox config directory")?;
    require_directory(&config.workspace, "sandbox workspace")?;
    require_directory(&config.state_dir, "sandbox state directory")?;
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
    fn create(config: &SandboxBuildConfig, idmap: &MbuildIdmap) -> Result<Self, RuntimeError> {
        let mut dirs = SandboxDirs::create(&config.workspace)?;
        let runtime_files = SandboxRuntimeFiles::create(&dirs.host_files)?;
        let cgroup = prepare_sandbox_cgroup()?;
        let spec = build_sandbox_spec(
            config,
            idmap,
            &mut dirs,
            &runtime_files,
            Some(cgroup.container_path),
        )?;
        let bundle = create_bundle(&config.workspace, &spec)?;
        populate_bundle_rootfs_skeleton(bundle.rootfs_dir(), &config.rootfs)?;
        Ok(Self {
            bundle,
            runtime_files,
            host_cgroup_path: cgroup.host_path,
        })
    }
}

struct SandboxDirs {
    root: PathBuf,
    rootfs_overlays: HashMap<String, OverlayDirs>,
    host_files: PathBuf,
}

struct OverlayDirs {
    upper: PathBuf,
    work: PathBuf,
}

impl SandboxDirs {
    fn create(workspace: &Path) -> Result<Self, RuntimeError> {
        let root = workspace
            .join("sandbox")
            .join(Uuid::new_v4().simple().to_string());
        let rootfs_overlays = root.join("rootfs-overlays");
        let host_files = root.join("host-files");

        fs::create_dir_all(&rootfs_overlays)?;
        fs::create_dir_all(&host_files)?;

        Ok(Self {
            root,
            rootfs_overlays: HashMap::new(),
            host_files,
        })
    }

    fn rootfs_overlay(&mut self, name: &str) -> Result<&OverlayDirs, RuntimeError> {
        if !self.rootfs_overlays.contains_key(name) {
            let root = self.root.join("rootfs-overlays").join(name);
            let upper = root.join("upper");
            let work = root.join("work");
            fs::create_dir_all(&upper)?;
            fs::create_dir_all(&work)?;
            self.rootfs_overlays
                .insert(name.to_string(), OverlayDirs { upper, work });
        }
        Ok(self
            .rootfs_overlays
            .get(name)
            .expect("rootfs overlay exists"))
    }
}

struct SandboxRuntimeFiles {
    hosts: PathBuf,
    resolv_conf: PathBuf,
    success_report: PathBuf,
    failure_report: PathBuf,
}

impl SandboxRuntimeFiles {
    fn create(root: &Path) -> Result<Self, RuntimeError> {
        let hosts = root.join("hosts");
        let resolv_conf = root.join("resolv.conf");
        let success_report = root.join("sandbox-success.json");
        let failure_report = root.join("sandbox-failure.json");
        fs::write(&hosts, "127.0.0.1 localhost mbuild\n::1 localhost mbuild\n")?;
        fs::write(&resolv_conf, "")?;
        File::create(&success_report)?;
        File::create(&failure_report)?;
        Ok(Self {
            hosts,
            resolv_conf,
            success_report,
            failure_report,
        })
    }
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
        runner: SandboxRunnerExecutor,
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
        let mut container = ContainerBuilder::new(container_id.clone(), SyscallType::Linux)
            .with_executor(runner)
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
struct SandboxRunnerExecutor {
    prepare_paths: Vec<PathBuf>,
    steps: Vec<SandboxRunnerStep>,
    success_report: Arc<File>,
    failure_report: Arc<File>,
}

impl SandboxRunnerExecutor {
    fn new(
        config: &SandboxBuildConfig,
        runtime_files: &SandboxRuntimeFiles,
    ) -> Result<Self, RuntimeError> {
        let steps = config
            .steps
            .iter()
            .map(SandboxRunnerStep::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            prepare_paths: vec![PathBuf::from("/__mbuild/build")],
            steps,
            success_report: Arc::new(File::create(&runtime_files.success_report)?),
            failure_report: Arc::new(File::create(&runtime_files.failure_report)?),
        })
    }

    fn run(&self) -> Result<SandboxBuildOutcome, SandboxRunnerFailureReport> {
        self.prepare()?;

        let mut reports = Vec::new();
        for step in &self.steps {
            reports.push(step.run()?);
        }

        let out_dir = Path::new("/__mbuild/out");
        let manifest = scan_output_manifest(out_dir)?;
        let manifest_bytes = manifest.to_canonical_bytes().map_err(|error| {
            SandboxRunnerFailureReport::runtime("sandbox-manifest-output", error.to_string())
        })?;
        let object_hash = hash_fs_tree_object(&manifest_bytes, out_dir).map_err(|error| {
            SandboxRunnerFailureReport::runtime("sandbox-hash-output", error.to_string())
        })?;
        Ok(SandboxBuildOutcome {
            object_hash,
            manifest,
            steps: reports,
        })
    }

    fn prepare(&self) -> Result<(), SandboxRunnerFailureReport> {
        for path in &self.prepare_paths {
            fs::create_dir_all(path).map_err(|error| prepare_error("create dir", path, error))?;
            chown_tree(path, BUILD_USER_UID, BUILD_USER_GID)
                .map_err(|error| prepare_error("chown", path, error))?;
        }
        Ok(())
    }

    fn write_success(&self, outcome: &SandboxBuildOutcome) -> Result<(), ExecutorError> {
        let manifest_jsonl = String::from_utf8(
            outcome
                .manifest
                .to_canonical_bytes()
                .map_err(executor_error)?,
        )
        .map_err(executor_error)?;
        let report = SandboxRunnerSuccessReport {
            object_hash: outcome.object_hash.to_string(),
            manifest_jsonl,
            steps: outcome.steps.clone(),
        };
        serde_json::to_writer(&*self.success_report, &report).map_err(executor_error)
    }

    fn write_failure(&self, report: &SandboxRunnerFailureReport) -> Result<(), ExecutorError> {
        serde_json::to_writer(&*self.failure_report, report).map_err(executor_error)
    }
}

impl Executor for SandboxRunnerExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        let exit_code = match self.run() {
            Ok(outcome) => {
                self.write_success(&outcome)?;
                0
            }
            Err(report) => {
                self.write_failure(&report)?;
                1
            }
        };
        terminate_remaining_children();
        std::process::exit(exit_code);
    }
}

#[derive(Clone)]
struct SandboxRunnerStep {
    name: String,
    run_as: SandboxRunAs,
    cwd: PathBuf,
    argv: Vec<String>,
    env: HashMap<String, String>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout: Arc<File>,
    stderr: Arc<File>,
}

impl SandboxRunnerStep {
    fn new(step: &SandboxStep) -> Result<Self, RuntimeError> {
        Ok(Self {
            name: step.name.clone(),
            run_as: step.run_as,
            cwd: step.cwd.clone(),
            argv: step.argv.clone(),
            env: step_env(step),
            stdout_path: step.stdout_path.clone(),
            stderr_path: step.stderr_path.clone(),
            stdout: Arc::new(File::create(&step.stdout_path)?),
            stderr: Arc::new(File::create(&step.stderr_path)?),
        })
    }

    fn run(&self) -> Result<SandboxStepReport, SandboxRunnerFailureReport> {
        let executable = self.argv.first().ok_or_else(|| {
            SandboxRunnerFailureReport::step_runtime(
                self,
                "step argument vector must contain at least one element".to_string(),
                0,
            )
        })?;
        let start = Instant::now();
        let stdout = self.stdout.try_clone().map_err(|error| {
            SandboxRunnerFailureReport::step_runtime(self, error.to_string(), elapsed_ms(start))
        })?;
        let stderr = self.stderr.try_clone().map_err(|error| {
            SandboxRunnerFailureReport::step_runtime(self, error.to_string(), elapsed_ms(start))
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
        unsafe {
            command.pre_exec(move || apply_step_credentials(run_as, setgroups_allowed));
        }

        let status = command.status().map_err(|error| {
            SandboxRunnerFailureReport::step_runtime(
                self,
                format!(
                    "failed to spawn '{}' as {}: {error}",
                    executable,
                    self.run_as.as_str()
                ),
                elapsed_ms(start),
            )
        })?;
        reap_finished_children();
        let duration_ms = elapsed_ms(start);
        if status.success() {
            Ok(SandboxStepReport {
                name: self.name.clone(),
                run_as: self.run_as.as_str().to_string(),
                exit_code: status.code().unwrap_or(0),
                duration_ms,
                stdout_path: self.stdout_path.clone(),
                stderr_path: self.stderr_path.clone(),
            })
        } else {
            Err(SandboxRunnerFailureReport::failed_step(
                self,
                &status,
                duration_ms,
            ))
        }
    }
}

impl SandboxRunAs {
    fn as_str(self) -> &'static str {
        match self {
            SandboxRunAs::BuildUser => "build-user",
            SandboxRunAs::Root => "root",
        }
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
    fn runtime(label: &str, message: String) -> Self {
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
            stdout_path: Some(step.stdout_path.clone()),
            stderr_path: Some(step.stderr_path.clone()),
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
            stdout_path: Some(step.stdout_path.clone()),
            stderr_path: Some(step.stderr_path.clone()),
        }
    }

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

fn scan_output_manifest(root: &Path) -> Result<FsTreeManifest, SandboxRunnerFailureReport> {
    let mut entries = Vec::new();
    scan_output_entry(root, "", &mut entries)?;
    FsTreeManifest::from_entries(entries).map_err(|error| {
        SandboxRunnerFailureReport::runtime("sandbox-manifest-output", error.to_string())
    })
}

fn scan_output_entry(
    path: &Path,
    rel_path: &str,
    entries: &mut Vec<FsTreeEntry>,
) -> Result<(), SandboxRunnerFailureReport> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        SandboxRunnerFailureReport::runtime(
            "sandbox-manifest-output",
            format!("failed to inspect '{}': {error}", path.display()),
        )
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
            SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("failed to read directory '{}': {error}", path.display()),
            )
        })?;
        for child in children {
            let child = child.map_err(|error| {
                SandboxRunnerFailureReport::runtime(
                    "sandbox-manifest-output",
                    format!(
                        "failed to read directory entry in '{}': {error}",
                        path.display()
                    ),
                )
            })?;
            let name = child.file_name();
            let name = name.to_str().ok_or_else(|| {
                SandboxRunnerFailureReport::runtime(
                    "sandbox-manifest-output",
                    format!("output path under '{}' is not UTF-8", path.display()),
                )
            })?;
            let child_rel_path = if rel_path.is_empty() {
                name.to_string()
            } else {
                format!("{rel_path}/{name}")
            };
            scan_output_entry(&child.path(), &child_rel_path, entries)?;
        }
    } else if file_type.is_file() {
        entries.push(FsTreeEntry::file(
            rel_path,
            uid,
            gid,
            metadata.permissions().mode() & 0o7777,
        ));
    } else if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|error| {
            SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("failed to read symlink '{}': {error}", path.display()),
            )
        })?;
        let target = target.to_str().ok_or_else(|| {
            SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("symlink target for '{}' is not UTF-8", path.display()),
            )
        })?;
        entries.push(FsTreeEntry::symlink(rel_path, uid, gid, target));
    } else {
        return Err(SandboxRunnerFailureReport::runtime(
            "sandbox-manifest-output",
            format!("unsupported output file type '{}'", path.display()),
        ));
    }

    Ok(())
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

fn prepare_error(operation: &str, path: &Path, error: io::Error) -> SandboxRunnerFailureReport {
    SandboxRunnerFailureReport::runtime(
        "sandbox-prepare",
        format!("{operation} '{}': {error}", path.display()),
    )
}

fn terminate_remaining_children() {
    unsafe {
        libc::kill(-1, libc::SIGTERM);
    }
    reap_finished_children();
    unsafe {
        libc::kill(-1, libc::SIGKILL);
    }
    reap_finished_children();
}

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

fn build_sandbox_spec(
    config: &SandboxBuildConfig,
    idmap: &MbuildIdmap,
    dirs: &mut SandboxDirs,
    host_files: &SandboxRuntimeFiles,
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

    let mut mounts = rootfs_top_level_mounts(&config.rootfs, dirs)?;
    mounts.extend([
        proc_mount()?,
        tmpfs_mount(Path::new("/tmp"), &["mode=1777"])?,
        tmpfs_mount(Path::new("/run"), &["mode=755"])?,
        bind_mount(&config.config_dir, Path::new("/__mbuild/config"), true)?,
        bind_mount(&config.out_dir, Path::new("/__mbuild/out"), false)?,
        bind_mount(&host_files.hosts, Path::new("/etc/hosts"), true)?,
        bind_mount(&host_files.resolv_conf, Path::new("/etc/resolv.conf"), true)?,
    ]);

    for input in &config.inputs {
        mounts.push(bind_mount(&input.host_path, &input.mount_path, true)?);
    }

    build_oci(
        SpecBuilder::default()
            .version("1.0.2")
            .hostname("mbuild")
            .root(build_oci(
                RootBuilder::default()
                    .path("rootfs")
                    .readonly(false)
                    .build(),
            )?)
            .process(build_oci(
                ProcessBuilder::default()
                    .terminal(false)
                    .user(build_oci(
                        UserBuilder::default().uid(0_u32).gid(0_u32).build(),
                    )?)
                    .args(vec!["mbuild-sandbox-init".to_string()])
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

fn rootfs_top_level_mounts(
    rootfs: &Path,
    dirs: &mut SandboxDirs,
) -> Result<Vec<Mount>, RuntimeError> {
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

        if file_type.is_dir() {
            if should_mount_rootfs_directory(name) {
                let overlay = dirs.rootfs_overlay(name)?;
                mounts.push(overlay_mount(
                    &destination,
                    &source,
                    &overlay.upper,
                    &overlay.work,
                )?);
            }
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

fn should_mount_rootfs_directory(name: &str) -> bool {
    !matches!(name, "dev" | "proc" | "run" | "tmp")
}

fn populate_bundle_rootfs_skeleton(
    bundle_rootfs: &Path,
    lower_rootfs: &Path,
) -> Result<(), RuntimeError> {
    for entry in rootfs_top_level_entries(lower_rootfs)? {
        let name = entry.file_name();
        let destination = bundle_rootfs.join(&name);
        let file_type = entry.file_type()?;

        if file_type.is_symlink() {
            let target = fs::read_link(entry.path())?;
            if !destination.exists() && !destination.is_symlink() {
                symlink(target, destination)?;
            }
        } else if file_type.is_dir() {
            let name = name.to_str().ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "sandbox rootfs '{}' contains non-UTF-8 top-level entry",
                    lower_rootfs.display()
                ))
            })?;
            if !should_mount_rootfs_directory(name) {
                fs::create_dir_all(destination)?;
            }
        }
    }
    Ok(())
}

fn overlay_mount(
    destination: &Path,
    lower: &Path,
    upper: &Path,
    work: &Path,
) -> Result<Mount, RuntimeError> {
    build_oci(
        MountBuilder::default()
            .destination(destination)
            .typ("overlay")
            .source(Path::new("overlay"))
            .options(vec![
                format!("lowerdir={}", lower.display()),
                format!("upperdir={}", upper.display()),
                format!("workdir={}", work.display()),
                "userxattr".to_string(),
            ])
            .build(),
    )
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

fn credential_error(operation: &str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{operation}: {error}"))
}

fn setgroups_allowed() -> bool {
    match fs::read_to_string("/proc/self/setgroups") {
        Ok(value) => setgroups_value_allows_setgroups(&value),
        Err(_) => true,
    }
}

fn setgroups_value_allows_setgroups(value: &str) -> bool {
    value.trim() != "deny"
}

fn apply_step_credentials(run_as: SandboxRunAs, setgroups_allowed: bool) -> io::Result<()> {
    match run_as {
        SandboxRunAs::BuildUser => {
            if setgroups_allowed {
                setgroups(&[]).map_err(|error| credential_error("setgroups([])", error))?;
            }
            setgid(Gid::from_raw(BUILD_USER_GID))
                .map_err(|error| credential_error("setgid(1)", error))?;
            setuid(Uid::from_raw(BUILD_USER_UID))
                .map_err(|error| credential_error("setuid(1)", error))
        }
        SandboxRunAs::Root => Ok(()),
    }
}

fn step_env(step: &SandboxStep) -> HashMap<String, String> {
    let mut env = HashMap::from([
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        ("HOME".to_string(), "/__mbuild/build".to_string()),
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
    use tempfile::tempdir;

    #[test]
    fn sandbox_spec_uses_top_level_rootfs_overlays_and_output_bind() {
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
        for name in ["dev", "etc", "proc", "run", "tmp", "usr"] {
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
        let mut dirs = SandboxDirs::create(&workspace).unwrap();
        let host_files = SandboxRuntimeFiles::create(&dirs.host_files).unwrap();
        let idmap = MbuildIdmap::for_tests(1000, 1001, 100000, 65536, 200000, 65536);

        let cgroup_path = PathBuf::from("/user.slice/mbuild-test.scope");
        let spec = build_sandbox_spec(
            &build_config,
            &idmap,
            &mut dirs,
            &host_files,
            Some(cgroup_path.clone()),
        )
        .unwrap();
        let mounts = spec.mounts().as_ref().unwrap();
        let linux = spec.linux().as_ref().unwrap();

        assert_eq!(linux.cgroups_path().as_ref(), Some(&cgroup_path));
        assert!(linux.resources().is_none());

        assert!(
            !mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/")
                    && mount.typ().as_deref() == Some("overlay"))
        );
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/dev")
                    && mount.typ().as_deref() == Some("overlay"))
        );
        let usr_overlay = mounts
            .iter()
            .find(|mount| {
                mount.destination() == Path::new("/usr")
                    && mount.typ().as_deref() == Some("overlay")
            })
            .expect("/usr overlay mount exists");
        let usr_overlay_options = usr_overlay.options().as_ref().unwrap();
        assert!(
            usr_overlay_options
                .iter()
                .any(|option| option == "userxattr")
        );
        assert!(
            !usr_overlay_options
                .iter()
                .any(|option| option == "metacopy=on")
        );
        assert!(
            mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/__mbuild/out")
                    && mount.source().as_deref() == Some(out.as_path()))
        );
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
        assert!(!mounts.iter().any(|mount| mount.destination()
            == Path::new("/__mbuild/inputs/source")
            && mount.typ().as_deref() == Some("overlay")));
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.destination()
                    == Path::new("/__mbuild/runtime/output-hash.json"))
        );
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
    fn sandbox_bundle_skeleton_copies_top_level_symlinks_and_skipped_dirs() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let bundle = temp.path().join("bundle");
        fs::create_dir_all(&lower).unwrap();
        fs::create_dir_all(&bundle).unwrap();
        fs::create_dir(lower.join("run")).unwrap();
        fs::create_dir(lower.join("usr")).unwrap();
        symlink("usr/bin", lower.join("bin")).unwrap();

        populate_bundle_rootfs_skeleton(&bundle, &lower).unwrap();

        assert_eq!(
            fs::read_link(bundle.join("bin")).unwrap(),
            Path::new("usr/bin")
        );
        assert!(bundle.join("run").is_dir());
        assert!(!bundle.join("usr").exists());
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
        symlink("file", root.join("link")).unwrap();
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
        assert!(manifest.entries().contains(&FsTreeEntry::file(
            "file",
            owner.uid(),
            owner.gid(),
            0o640
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::file(
            "exe",
            owner.uid(),
            owner.gid(),
            0o755
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::symlink(
            "link",
            owner.uid(),
            owner.gid(),
            "file"
        )));
    }

    #[test]
    fn setgroups_value_detects_rootless_deny() {
        assert!(!setgroups_value_allows_setgroups("deny\n"));
        assert!(setgroups_value_allows_setgroups("allow\n"));
        assert!(setgroups_value_allows_setgroups(""));
    }
}
