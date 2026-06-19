//! In-namespace runner: executes the ordered steps as pid 1 and writes the
//! success/failure reports.

use crate::protocol::{
    RunnerConfig, RunnerRunAs, RunnerStepConfig, SANDBOX_PROTOCOL_VERSION, SandboxRunnerFailureReport,
    SandboxRunnerSuccessReport, SandboxStepReport,
};
#[cfg(not(test))]
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Gid, Uid, chown, setgid, setgroups, setuid};
#[cfg(not(test))]
use nix::unistd::Pid;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
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
    if config.protocol_version != SANDBOX_PROTOCOL_VERSION {
        let report = SandboxRunnerFailureReport::runtime(
            "sandbox-protocol",
            format!(
                "unsupported sandbox protocol {}; expected {}",
                config.protocol_version, SANDBOX_PROTOCOL_VERSION
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
    success_report: File,
    failure_report: File,
}

type RunnerResult<T> = Result<T, Box<SandboxRunnerFailureReport>>;

fn runner_error(report: SandboxRunnerFailureReport) -> Box<SandboxRunnerFailureReport> {
    Box::new(report)
}

impl SandboxRunner {
    fn new(config: RunnerConfig) -> io::Result<Self> {
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
            Err(nix::errno::Errno::EINTR) => continue,
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
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn run_config_rejects_protocol_mismatch_with_failure_report() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path()).with_protocol(SANDBOX_PROTOCOL_VERSION + 1);

        let exit_code = run_config(config.clone());

        assert_eq!(exit_code, 1);
        let report = read_failure_report(&config.failure_report);
        assert_eq!(report.label, "sandbox-protocol");
        assert!(
            report.message.contains("unsupported sandbox protocol"),
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

        let exit_code = run_config(config.clone());

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
}
