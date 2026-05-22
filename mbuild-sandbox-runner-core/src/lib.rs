use fsobj_hash::{ObjectHash, hash_fs_tree_object, hash_path, hash_symlink_node};
use mbuild_core::{FsTreeEntry, FsTreeManifest};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Gid, Pid, Uid, chown, setgid, setgroups, setuid};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const RUNNER_BINARY_NAME: &str = "mbuild-sandbox-runner";
pub const RUNNER_PROTOCOL_VERSION: u32 = 1;

const BUILD_USER_UID: u32 = 1;
const BUILD_USER_GID: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerProtocolInfo {
    pub name: String,
    pub protocol_version: u32,
}

pub fn protocol_info() -> RunnerProtocolInfo {
    RunnerProtocolInfo {
        name: RUNNER_BINARY_NAME.to_string(),
        protocol_version: RUNNER_PROTOCOL_VERSION,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub protocol_version: u32,
    pub prepare_paths: Vec<PathBuf>,
    pub steps: Vec<RunnerStepConfig>,
    pub output_dir: PathBuf,
    pub success_report: PathBuf,
    pub failure_report: PathBuf,
    pub breadcrumbs: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerStepConfig {
    pub name: String,
    pub run_as: RunnerRunAs,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub report_stdout_path: PathBuf,
    pub report_stderr_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerRunAs {
    BuildUser,
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
    pub object_hash: String,
    pub manifest_jsonl: String,
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
    success_report: Arc<File>,
    failure_report: Arc<File>,
    breadcrumbs: Arc<File>,
}

impl SandboxRunner {
    fn new(config: RunnerConfig) -> io::Result<Self> {
        let success_report = Arc::new(File::create(&config.success_report)?);
        let failure_report = Arc::new(File::create(&config.failure_report)?);
        let breadcrumbs = Arc::new(File::create(&config.breadcrumbs)?);
        let steps = config
            .steps
            .into_iter()
            .map(|step| SandboxRunnerStep::new(step, Arc::clone(&breadcrumbs)))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            prepare_paths: config.prepare_paths,
            steps,
            output_dir: config.output_dir,
            success_report,
            failure_report,
            breadcrumbs,
        })
    }

    fn exec(&self) -> i32 {
        self.breadcrumb("executor:exec:start");
        let exit_code = match self.run() {
            Ok(outcome) => {
                self.breadcrumb("executor:run:ok");
                if let Err(error) = self.write_success(&outcome) {
                    eprintln!("{error}");
                    1
                } else {
                    0
                }
            }
            Err(report) => {
                self.breadcrumb(&format!("executor:run:error:{}", report.to_error_message()));
                if let Err(error) = self.write_failure(&report) {
                    eprintln!("{error}");
                }
                1
            }
        };
        self.breadcrumb("terminate-remaining-children:start");
        terminate_remaining_children();
        self.breadcrumb(&format!("process-exit:{exit_code}"));
        exit_code
    }

    fn run(&self) -> Result<SandboxRunnerOutcome, SandboxRunnerFailureReport> {
        self.breadcrumb("run:start");
        self.breadcrumb("prepare:start");
        self.prepare()?;
        self.breadcrumb("prepare:done");

        let mut reports = Vec::new();
        for step in &self.steps {
            self.breadcrumb(&format!("step:{}:run:start", step.name));
            reports.push(step.run()?);
            self.breadcrumb(&format!("step:{}:run:done", step.name));
        }

        self.breadcrumb("manifest:scan:start");
        let manifest = scan_output_manifest(&self.output_dir)?;
        self.breadcrumb("manifest:scan:done");
        self.breadcrumb("manifest:canonical:start");
        let manifest_bytes = manifest.to_canonical_bytes().map_err(|error| {
            SandboxRunnerFailureReport::runtime("sandbox-manifest-output", error.to_string())
        })?;
        self.breadcrumb("manifest:canonical:done");
        self.breadcrumb("hash-output:start");
        let object_hash =
            hash_fs_tree_object(&manifest_bytes, &self.output_dir).map_err(|error| {
                SandboxRunnerFailureReport::runtime("sandbox-hash-output", error.to_string())
            })?;
        self.breadcrumb("hash-output:done");
        Ok(SandboxRunnerOutcome {
            object_hash,
            manifest,
            steps: reports,
        })
    }

    fn prepare(&self) -> Result<(), SandboxRunnerFailureReport> {
        for path in &self.prepare_paths {
            self.breadcrumb(&format!("prepare:create-dir:start:{}", path.display()));
            fs::create_dir_all(path).map_err(|error| prepare_error("create dir", path, error))?;
            self.breadcrumb(&format!("prepare:create-dir:done:{}", path.display()));
            self.breadcrumb(&format!("prepare:chown:start:{}", path.display()));
            chown_tree(path, BUILD_USER_UID, BUILD_USER_GID)
                .map_err(|error| prepare_error("chown", path, error))?;
            self.breadcrumb(&format!("prepare:chown:done:{}", path.display()));
        }
        Ok(())
    }

    fn write_success(&self, outcome: &SandboxRunnerOutcome) -> io::Result<()> {
        self.breadcrumb("write-success:start");
        let manifest_jsonl = String::from_utf8(
            outcome
                .manifest
                .to_canonical_bytes()
                .map_err(io::Error::other)?,
        )
        .map_err(io::Error::other)?;
        let report = SandboxRunnerSuccessReport {
            object_hash: outcome.object_hash.to_string(),
            manifest_jsonl,
            steps: outcome.steps.clone(),
        };
        serde_json::to_writer(&*self.success_report, &report).map_err(io::Error::other)?;
        self.breadcrumb("write-success:done");
        Ok(())
    }

    fn write_failure(&self, report: &SandboxRunnerFailureReport) -> io::Result<()> {
        self.breadcrumb(&format!(
            "write-failure:start:{}",
            report.to_error_message()
        ));
        serde_json::to_writer(&*self.failure_report, report).map_err(io::Error::other)?;
        self.breadcrumb("write-failure:done");
        Ok(())
    }

    fn breadcrumb(&self, message: &str) {
        write_breadcrumb(&self.breadcrumbs, message);
    }
}

struct SandboxRunnerOutcome {
    object_hash: ObjectHash,
    manifest: FsTreeManifest,
    steps: Vec<SandboxStepReport>,
}

#[derive(Clone)]
struct SandboxRunnerStep {
    name: String,
    run_as: RunnerRunAs,
    cwd: PathBuf,
    argv: Vec<String>,
    env: HashMap<String, String>,
    report_stdout_path: PathBuf,
    report_stderr_path: PathBuf,
    stdout: Arc<File>,
    stderr: Arc<File>,
    breadcrumbs: Arc<File>,
}

impl SandboxRunnerStep {
    fn new(step: RunnerStepConfig, breadcrumbs: Arc<File>) -> io::Result<Self> {
        Ok(Self {
            name: step.name,
            run_as: step.run_as,
            cwd: step.cwd,
            argv: step.argv,
            env: step.env,
            report_stdout_path: step.report_stdout_path,
            report_stderr_path: step.report_stderr_path,
            stdout: Arc::new(File::create(&step.stdout_path)?),
            stderr: Arc::new(File::create(&step.stderr_path)?),
            breadcrumbs,
        })
    }

    fn run(&self) -> Result<SandboxStepReport, SandboxRunnerFailureReport> {
        self.breadcrumb("start");
        let executable = self.argv.first().ok_or_else(|| {
            SandboxRunnerFailureReport::step_runtime(
                self,
                "step argument vector must contain at least one element".to_string(),
                0,
            )
        })?;
        self.breadcrumb(&format!(
            "argv-ready: executable={} argc={} cwd={} run_as={}",
            executable,
            self.argv.len(),
            self.cwd.display(),
            self.run_as.as_str()
        ));
        let start = Instant::now();
        self.breadcrumb("stdout-clone:start");
        let stdout = self.stdout.try_clone().map_err(|error| {
            SandboxRunnerFailureReport::step_runtime(self, error.to_string(), elapsed_ms(start))
        })?;
        self.breadcrumb("stdout-clone:done");
        self.breadcrumb("stderr-clone:start");
        let stderr = self.stderr.try_clone().map_err(|error| {
            SandboxRunnerFailureReport::step_runtime(self, error.to_string(), elapsed_ms(start))
        })?;
        self.breadcrumb("stderr-clone:done");
        self.breadcrumb("pre-exec-breadcrumbs-clone:start");
        let pre_exec_breadcrumbs = self.breadcrumbs.try_clone().map_err(|error| {
            SandboxRunnerFailureReport::step_runtime(self, error.to_string(), elapsed_ms(start))
        })?;
        self.breadcrumb("pre-exec-breadcrumbs-clone:done");

        self.breadcrumb("command-build:start");
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
            command.pre_exec(move || {
                apply_step_credentials(run_as, setgroups_allowed, pre_exec_breadcrumbs.as_raw_fd())
            });
        }
        self.breadcrumb(&format!(
            "command-build:done setgroups_allowed={setgroups_allowed}"
        ));

        self.breadcrumb("command-spawn:start");
        let mut child = command.spawn().map_err(|error| {
            self.breadcrumb(&format!("command-spawn:error:{error}"));
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
        let child_pid = child.id();
        self.breadcrumb(&format!("command-spawn:done child_pid={child_pid}"));

        self.breadcrumb(&format!("command-wait:start child_pid={child_pid}"));
        let status = child.wait().map_err(|error| {
            self.breadcrumb(&format!(
                "command-wait:error child_pid={child_pid} error={error}"
            ));
            SandboxRunnerFailureReport::step_runtime(
                self,
                format!(
                    "failed to wait for '{}' as {} child pid {}: {error}",
                    executable,
                    self.run_as.as_str(),
                    child_pid
                ),
                elapsed_ms(start),
            )
        })?;
        self.breadcrumb(&format!(
            "command-wait:done child_pid={child_pid} status={status:?}"
        ));
        self.breadcrumb("reap-children:start");
        reap_finished_children();
        self.breadcrumb("reap-children:done");
        let duration_ms = elapsed_ms(start);
        if status.success() {
            self.breadcrumb(&format!("success:duration_ms={duration_ms}"));
            Ok(SandboxStepReport {
                name: self.name.clone(),
                run_as: self.run_as.as_str().to_string(),
                exit_code: status.code().unwrap_or(0),
                duration_ms,
                stdout_path: self.report_stdout_path.clone(),
                stderr_path: self.report_stderr_path.clone(),
            })
        } else {
            self.breadcrumb(&format!(
                "failed:duration_ms={duration_ms} status={status:?}"
            ));
            Err(SandboxRunnerFailureReport::failed_step(
                self,
                &status,
                duration_ms,
            ))
        }
    }

    fn breadcrumb(&self, message: &str) {
        write_breadcrumb(&self.breadcrumbs, &format!("step:{}:{message}", self.name));
    }
}

fn write_breadcrumb(file: &File, message: &str) {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| format!("{}.{:09}", duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or_else(|_| "time-error".to_string());
    let mut file = file;
    let _ = writeln!(file, "{elapsed} pid={} {message}", std::process::id());
    let _ = file.flush();
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
        let hash = hash_path(path).map_err(|error| {
            SandboxRunnerFailureReport::runtime(
                "sandbox-manifest-output",
                format!("failed to hash output file '{}': {error}", path.display()),
            )
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
        entries.push(FsTreeEntry::symlink_with_hash(
            rel_path,
            uid,
            gid,
            target,
            hash_symlink_node(target.as_bytes()),
        ));
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
    #[cfg(test)]
    {
        reap_finished_children();
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

fn setgroups_allowed() -> bool {
    match fs::read_to_string("/proc/self/setgroups") {
        Ok(value) => value.trim() != "deny",
        Err(_) => true,
    }
}

fn raw_pre_exec_breadcrumb(fd: RawFd, message: &'static [u8]) {
    unsafe {
        let _ = libc::write(fd, message.as_ptr().cast(), message.len());
    }
}

fn apply_step_credentials(
    run_as: RunnerRunAs,
    setgroups_allowed: bool,
    breadcrumbs_fd: RawFd,
) -> io::Result<()> {
    raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:start\n");
    match run_as {
        RunnerRunAs::BuildUser => {
            if setgroups_allowed {
                raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setgroups:start\n");
                if let Err(error) = setgroups(&[]) {
                    raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setgroups:error\n");
                    return Err(credential_error("setgroups([])", error));
                }
                raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setgroups:done\n");
            } else {
                raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setgroups:skipped\n");
            }
            raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setgid:start\n");
            if let Err(error) = setgid(Gid::from_raw(BUILD_USER_GID)) {
                raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setgid:error\n");
                return Err(credential_error("setgid(1)", error));
            }
            raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setgid:done\n");
            raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setuid:start\n");
            if let Err(error) = setuid(Uid::from_raw(BUILD_USER_UID)) {
                raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setuid:error\n");
                return Err(credential_error("setuid(1)", error));
            }
            raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:setuid:done\n");
            raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:done\n");
            Ok(())
        }
        RunnerRunAs::Root => {
            raw_pre_exec_breadcrumb(breadcrumbs_fd, b"pre-exec:root:done\n");
            Ok(())
        }
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
    use tempfile::tempdir;

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
        let manifest =
            FsTreeManifest::parse_canonical_bytes(report.manifest_jsonl.as_bytes()).unwrap();
        assert!(
            manifest
                .entries()
                .iter()
                .any(|entry| matches!(entry, FsTreeEntry::File { path, .. } if path == "file"))
        );
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

        fn with_step(
            mut self,
            name: &str,
            argv: Vec<String>,
            env: HashMap<String, String>,
            stdout_path: PathBuf,
            stderr_path: PathBuf,
        ) -> RunnerConfig {
            self.config.steps.push(RunnerStepConfig {
                name: name.to_string(),
                run_as: RunnerRunAs::Root,
                cwd: PathBuf::from("/"),
                argv,
                env,
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
                report_stdout_path: stdout_path,
                report_stderr_path: stderr_path,
            });
            self.config
        }
    }

    fn test_config(root: &Path) -> TestConfig {
        TestConfig {
            config: RunnerConfig {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                prepare_paths: Vec::new(),
                steps: Vec::new(),
                output_dir: root.join("out"),
                success_report: root.join("success.json"),
                failure_report: root.join("failure.json"),
                breadcrumbs: root.join("breadcrumbs.log"),
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
