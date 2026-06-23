//! In-namespace runner: executes the ordered steps as pid 1 and writes the
//! success/failure reports.

use crate::protocol::{
    RunnerConfig, RunnerRunAs, RunnerStepConfig, SANDBOX_PROTOCOL_VERSION,
    SandboxRunnerFailureReport, SandboxRunnerSuccessReport, SandboxStepReport,
};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::Mode;
use nix::sys::wait::WaitStatus;
#[cfg(not(test))]
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::{Gid, Pid, Uid, fchownat, setgid, setgroups, setuid};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

const BUILD_USER_UID: u32 = 1;
const BUILD_USER_GID: u32 = 1;

/// Step-coupled failure-report constructors. The pure constructors live in
/// `protocol`; these reference [`SandboxRunnerStep`] so they belong here.
impl SandboxRunnerFailureReport {
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
        let (message, exit_code, signal) = match classify_exit_status(status) {
            StepExit::Exited(code) => (
                format!(
                    "sandbox step '{}' failed with exit status {code}",
                    step.name
                ),
                Some(code),
                None,
            ),
            StepExit::Signaled(signal) => (
                format!("sandbox step '{}' was killed by signal {signal}", step.name),
                None,
                Some(signal),
            ),
            StepExit::Unknown => (
                format!("sandbox step '{}' ended with status {status:?}", step.name),
                None,
                None,
            ),
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
}

/// Outcome of running the runner config.
///
/// The distinction matters for who writes the failure report. Once
/// `SandboxRunner::new` succeeds the runner owns its report files and writes
/// them itself ([`RunnerOutcome::Reported`]). Any failure *before* that point
/// ([`RunnerOutcome::EarlyFailure`]) leaves the report unwritten, so the
/// launcher records it through its pre-opened failure-report fd (the same file
/// on disk, opened before chroot).
pub enum RunnerOutcome {
    /// The runner owns its reports and has written one. This is the final code.
    Reported(i32),
    /// The runner failed before owning its reports; the launcher must record it.
    EarlyFailure(SandboxRunnerFailureReport),
}

pub fn run_config_path(path: &Path) -> RunnerOutcome {
    let config = match fs::read(path)
        .map_err(|error| format!("failed to read runner config '{}': {error}", path.display()))
        .and_then(|bytes| serde_json::from_slice::<RunnerConfig>(&bytes).map_err(|e| e.to_string()))
    {
        Ok(config) => config,
        Err(error) => {
            return RunnerOutcome::EarlyFailure(SandboxRunnerFailureReport::runtime(
                "runner-config",
                error,
            ));
        }
    };

    run_config(config)
}

pub fn run_config(config: RunnerConfig) -> RunnerOutcome {
    if config.protocol_version != SANDBOX_PROTOCOL_VERSION {
        return RunnerOutcome::EarlyFailure(SandboxRunnerFailureReport::runtime(
            "sandbox-protocol",
            format!(
                "unsupported sandbox protocol {}; expected {}",
                config.protocol_version, SANDBOX_PROTOCOL_VERSION
            ),
        ));
    }
    let runner = match SandboxRunner::new(config) {
        Ok(runner) => runner,
        Err(error) => {
            return RunnerOutcome::EarlyFailure(SandboxRunnerFailureReport::runtime(
                "sandbox-runner-init",
                error.to_string(),
            ));
        }
    };
    RunnerOutcome::Reported(runner.exec())
}

struct SandboxRunner {
    prepare_paths: Vec<PathBuf>,
    steps: Vec<SandboxRunnerStep>,
    success_report: File,
    failure_report: File,
}

type RunnerResult<T> = Result<T, Box<SandboxRunnerFailureReport>>;

fn runner_error(report: SandboxRunnerFailureReport) -> Box<SandboxRunnerFailureReport> {
    Box::new(report)
}

impl SandboxRunner {
    fn new(config: RunnerConfig) -> io::Result<Self> {
        // Both reports are created up front on purpose: this truncates any stale
        // report from a previous run and fails fast on permission problems. As a
        // result the "opposite" report is left empty (e.g. an empty failure
        // report on success). The authoritative signal is the process exit code,
        // not the existence or non-emptiness of either file; the runtime treats
        // an empty report as a fallback (see bobr-sandbox reports/lifecycle).
        let success_report = File::create(&config.success_report)?;
        let failure_report = File::create(&config.failure_report)?;
        let steps = config
            .steps
            .into_iter()
            .map(SandboxRunnerStep::new)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            prepare_paths: config.prepare_paths,
            steps,
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

        Ok(SandboxRunnerOutcome { steps: reports })
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
            steps: outcome.steps.clone(),
        };
        serde_json::to_writer(&self.success_report, &report).map_err(io::Error::other)?;
        Ok(())
    }

    fn write_failure(&self, report: &SandboxRunnerFailureReport) -> io::Result<()> {
        serde_json::to_writer(&self.failure_report, report).map_err(io::Error::other)
    }
}

struct SandboxRunnerOutcome {
    steps: Vec<SandboxStepReport>,
}

struct SandboxRunnerStep {
    name: String,
    run_as: RunnerRunAs,
    cwd: PathBuf,
    argv: Vec<String>,
    env: HashMap<String, String>,
    umask: u32,
    report_stdout_path: PathBuf,
    report_stderr_path: PathBuf,
    stdout: File,
    stderr: File,
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
            stdout: File::create(&step.stdout_path)?,
            stderr: File::create(&step.stderr_path)?,
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

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

/// How a finished step process terminated, used to build its failure report.
#[derive(Debug, PartialEq)]
enum StepExit {
    Exited(i32),
    Signaled(i32),
    Unknown,
}

fn classify_exit_status(status: &ExitStatus) -> StepExit {
    if let Some(code) = status.code() {
        StepExit::Exited(code)
    } else if let Some(signal) = status.signal() {
        StepExit::Signaled(signal)
    } else {
        StepExit::Unknown
    }
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

/// Whether the reap loop should keep draining children or stop.
enum ReapAction {
    Continue,
    Stop,
}

/// Pure decision for one `waitpid(-1, WNOHANG)` result: retry on `EINTR`,
/// keep draining while children are reaped, and stop once none are ready
/// (`StillAlive`), there are none left (`ECHILD`), or on any other error.
fn reap_action(result: Result<WaitStatus, Errno>) -> ReapAction {
    match result {
        Ok(WaitStatus::StillAlive) => ReapAction::Stop,
        Ok(_) => ReapAction::Continue,
        Err(Errno::EINTR) => ReapAction::Continue,
        Err(Errno::ECHILD) => ReapAction::Stop,
        Err(_) => ReapAction::Stop,
    }
}

#[cfg(not(test))]
fn reap_finished_children() {
    while let ReapAction::Continue =
        reap_action(waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)))
    {}
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
    let owner = Some(Uid::from_raw(uid));
    let group = Some(Gid::from_raw(gid));
    // Chown the top entry itself without dereferencing it as a symlink.
    fchownat(None, path, owner, group, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io::Error::other)?;
    // Recurse only into a real directory, descending by file descriptor with
    // O_NOFOLLOW so a swapped symlink can never redirect a chown out of the
    // tree (defense-in-depth against TOCTOU).
    if fs::symlink_metadata(path)?.is_dir() {
        let dir = open_dir_nofollow(None, path)?;
        chown_dir_contents(dir, owner, group)?;
    }
    Ok(())
}

fn chown_dir_contents(mut dir: Dir, owner: Option<Uid>, group: Option<Gid>) -> io::Result<()> {
    let dir_fd = dir.as_raw_fd();
    for entry in dir.iter() {
        let entry = entry.map_err(io::Error::other)?;
        let name = entry.file_name();
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        fchownat(
            Some(dir_fd),
            name,
            owner,
            group,
            AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .map_err(io::Error::other)?;
        // Descend only into real subdirectories: O_NOFOLLOW makes a symlink
        // entry fail with ELOOP and a non-directory fail with ENOTDIR.
        match Dir::openat(Some(dir_fd), name, dir_open_flags(), Mode::empty()) {
            Ok(subdir) => chown_dir_contents(subdir, owner, group)?,
            Err(Errno::ENOTDIR | Errno::ELOOP) => {}
            Err(error) => return Err(io::Error::other(error)),
        }
    }
    Ok(())
}

fn open_dir_nofollow(dirfd: Option<std::os::fd::RawFd>, path: &Path) -> io::Result<Dir> {
    Dir::openat(dirfd, path, dir_open_flags(), Mode::empty()).map_err(io::Error::other)
}

fn dir_open_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn run_config_rejects_protocol_mismatch_as_early_failure() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path()).with_protocol(SANDBOX_PROTOCOL_VERSION + 1);

        let report = early_failure(run_config(config));

        assert_eq!(report.label, "sandbox-protocol");
        assert!(
            report.message.contains("unsupported sandbox protocol"),
            "{}",
            report.message
        );
    }

    #[test]
    fn run_config_reports_runner_init_failure_as_early_failure() {
        let temp = tempdir().unwrap();
        let mut config = test_config(temp.path()).with_protocol(SANDBOX_PROTOCOL_VERSION);
        // A report path under a missing directory makes File::create in
        // SandboxRunner::new fail before the runner owns any report.
        config.success_report = temp.path().join("missing-dir").join("success.json");

        let report = early_failure(run_config(config));

        assert_eq!(report.label, "sandbox-runner-init");
    }

    #[test]
    fn run_config_executes_root_step_and_writes_success_report() {
        let temp = tempdir().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let config = test_config(temp.path()).with_step(
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

        let exit_code = reported_code(run_config(config.clone()));

        assert_eq!(exit_code, 0);
        assert_eq!(fs::read_to_string(output.join("file")).unwrap(), "payload");
        assert_eq!(fs::read_to_string(stdout).unwrap(), "hello");
        let report = read_success_report(&config.success_report);
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "write-output");
    }

    #[test]
    fn run_config_writes_failure_report_for_failed_step() {
        let temp = tempdir().unwrap();
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let config = test_config(temp.path()).with_step(
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

        let exit_code = reported_code(run_config(config.clone()));

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
        let config = test_config(temp.path()).with_step_umask(
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

        let exit_code = reported_code(run_config(config));

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
        let stdout = temp.path().join("stdout.log");
        let stderr = temp.path().join("stderr.log");
        let config = test_config(temp.path()).with_step_umask(
            "bad-umask",
            vec!["true".to_string()],
            HashMap::new(),
            0o1000,
            stdout,
            stderr,
        );

        // An invalid umask is rejected while building the step inside
        // SandboxRunner::new, before the runner owns its reports.
        let report = early_failure(run_config(config));

        assert_eq!(report.label, "sandbox-runner-init");
    }

    #[test]
    fn run_config_path_rejects_malformed_json_as_early_failure() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("runner-config.json");
        fs::write(&path, "not json").unwrap();

        let report = early_failure(run_config_path(&path));

        assert_eq!(report.label, "runner-config");
    }

    #[test]
    fn classify_exit_status_distinguishes_exit_and_signal() {
        use std::os::unix::process::ExitStatusExt;

        // 7 << 8 encodes "exited with status 7"; 9 encodes "killed by signal 9".
        assert_eq!(
            classify_exit_status(&ExitStatus::from_raw(7 << 8)),
            StepExit::Exited(7)
        );
        assert_eq!(
            classify_exit_status(&ExitStatus::from_raw(9)),
            StepExit::Signaled(9)
        );
    }

    #[test]
    fn reap_action_classifies_waitpid_results() {
        assert!(matches!(
            reap_action(Ok(WaitStatus::StillAlive)),
            ReapAction::Stop
        ));
        assert!(matches!(
            reap_action(Ok(WaitStatus::Exited(Pid::from_raw(123), 0))),
            ReapAction::Continue
        ));
        assert!(matches!(
            reap_action(Err(Errno::EINTR)),
            ReapAction::Continue
        ));
        assert!(matches!(reap_action(Err(Errno::ECHILD)), ReapAction::Stop));
        assert!(matches!(reap_action(Err(Errno::EINVAL)), ReapAction::Stop));
    }

    #[test]
    fn chown_tree_walks_without_following_symlinks() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/file"), b"x").unwrap();
        std::os::unix::fs::symlink("/nonexistent", root.join("sub/link")).unwrap();

        // Chowning to our own ids needs no privileges; this exercises the
        // fd-based descent, including the symlink (ELOOP) and file (ENOTDIR)
        // branches, without dereferencing the dangling symlink.
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        chown_tree(&root, uid, gid).unwrap();
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
                protocol_version: SANDBOX_PROTOCOL_VERSION,
                prepare_paths: Vec::new(),
                steps: Vec::new(),
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

    fn reported_code(outcome: RunnerOutcome) -> i32 {
        match outcome {
            RunnerOutcome::Reported(code) => code,
            RunnerOutcome::EarlyFailure(report) => {
                panic!(
                    "expected Reported, got EarlyFailure: {}",
                    report.to_error_message()
                )
            }
        }
    }

    fn early_failure(outcome: RunnerOutcome) -> SandboxRunnerFailureReport {
        match outcome {
            RunnerOutcome::EarlyFailure(report) => report,
            RunnerOutcome::Reported(code) => panic!("expected EarlyFailure, got Reported({code})"),
        }
    }
}
