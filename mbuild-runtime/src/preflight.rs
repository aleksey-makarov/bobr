//! Host preflight checks for runtime-backed helper operations.

use crate::{error::RuntimeError, idmap::MbuildIdmap};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BUSCTL_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn preflight_ownership_runtime(idmap: &MbuildIdmap) -> Result<(), RuntimeError> {
    check_libcontainer_ownership_runtime_preflight(idmap, &HostPreflightProbe)
}

pub(crate) fn preflight_local_helper_runtime(idmap: &MbuildIdmap) -> Result<(), RuntimeError> {
    check_local_helper_runtime_preflight(idmap, &HostPreflightProbe)
}

fn check_local_helper_runtime_preflight(
    idmap: &MbuildIdmap,
    probe: &impl PreflightProbe,
) -> Result<(), RuntimeError> {
    let mut failures = Vec::new();

    check_idmap(idmap, &mut failures);
    check_user_namespace_sysctl(probe, &mut failures);
    check_command_in_path(probe, "newuidmap", &mut failures);
    check_command_in_path(probe, "newgidmap", &mut failures);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::Preflight(failures.join("; ")))
    }
}

fn check_libcontainer_ownership_runtime_preflight(
    idmap: &MbuildIdmap,
    probe: &impl PreflightProbe,
) -> Result<(), RuntimeError> {
    let mut failures = Vec::new();

    check_idmap(idmap, &mut failures);
    check_path_is_dir(
        probe,
        Path::new("/sys/fs/cgroup"),
        "cgroup filesystem",
        &mut failures,
    );
    check_path_is_file(
        probe,
        Path::new("/sys/fs/cgroup/cgroup.controllers"),
        "cgroup v2 controllers file",
        &mut failures,
    );
    check_path_is_dir(
        probe,
        Path::new("/run/systemd/system"),
        "systemd runtime directory",
        &mut failures,
    );
    check_user_namespace_sysctl(probe, &mut failures);
    check_user_bus(probe, &mut failures);
    check_busctl(probe, &mut failures);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::Preflight(failures.join("; ")))
    }
}

fn check_idmap(idmap: &MbuildIdmap, failures: &mut Vec<String>) {
    if idmap.subuid_count() == 0 {
        failures.push("mbuild idmap has empty subuid range".to_string());
    }
    if idmap.subgid_count() == 0 {
        failures.push("mbuild idmap has empty subgid range".to_string());
    }
}

fn check_path_is_dir(
    probe: &impl PreflightProbe,
    path: &Path,
    label: &str,
    failures: &mut Vec<String>,
) {
    match probe.path_kind(path) {
        Ok(PathKind::Directory) => {}
        Ok(PathKind::Missing) => failures.push(format!("{label} '{}' is missing", path.display())),
        Ok(kind) => failures.push(format!(
            "{label} '{}' is not a directory ({kind})",
            path.display()
        )),
        Err(error) => failures.push(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        )),
    }
}

fn check_path_is_file(
    probe: &impl PreflightProbe,
    path: &Path,
    label: &str,
    failures: &mut Vec<String>,
) {
    match probe.path_kind(path) {
        Ok(PathKind::File) => {}
        Ok(PathKind::Missing) => failures.push(format!("{label} '{}' is missing", path.display())),
        Ok(kind) => failures.push(format!(
            "{label} '{}' is not a file ({kind})",
            path.display()
        )),
        Err(error) => failures.push(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        )),
    }
}

fn check_user_namespace_sysctl(probe: &impl PreflightProbe, failures: &mut Vec<String>) {
    let max_user_namespaces = Path::new("/proc/sys/user/max_user_namespaces");
    match probe.read_to_string(max_user_namespaces) {
        Ok(value) => check_positive_sysctl(
            max_user_namespaces,
            &value,
            "unprivileged user namespaces",
            failures,
        ),
        Err(error) => failures.push(format!(
            "failed to read user namespace sysctl '{}': {error}",
            max_user_namespaces.display()
        )),
    }

    let unprivileged_clone = Path::new("/proc/sys/kernel/unprivileged_userns_clone");
    match probe.path_kind(unprivileged_clone) {
        Ok(PathKind::Missing) => {}
        Ok(PathKind::File) => match probe.read_to_string(unprivileged_clone) {
            Ok(value) => check_positive_sysctl(
                unprivileged_clone,
                &value,
                "unprivileged user namespace clone",
                failures,
            ),
            Err(error) => failures.push(format!(
                "failed to read user namespace clone sysctl '{}': {error}",
                unprivileged_clone.display()
            )),
        },
        Ok(kind) => failures.push(format!(
            "user namespace clone sysctl '{}' is not a file ({kind})",
            unprivileged_clone.display()
        )),
        Err(error) => failures.push(format!(
            "failed to inspect user namespace clone sysctl '{}': {error}",
            unprivileged_clone.display()
        )),
    }
}

fn check_positive_sysctl(path: &Path, value: &str, label: &str, failures: &mut Vec<String>) {
    match value.trim().parse::<u64>() {
        Ok(value) if value > 0 => {}
        Ok(_) => failures.push(format!("{label} disabled: '{}' is 0", path.display())),
        Err(error) => failures.push(format!(
            "failed to parse {label} sysctl '{}': {error}",
            path.display()
        )),
    }
}

fn check_command_in_path(probe: &impl PreflightProbe, name: &str, failures: &mut Vec<String>) {
    match probe.command_in_path(name) {
        Ok(true) => {}
        Ok(false) => failures.push(format!("{name} not found in PATH")),
        Err(error) => failures.push(format!("failed to inspect PATH for {name}: {error}")),
    }
}

fn check_user_bus(probe: &impl PreflightProbe, failures: &mut Vec<String>) {
    if probe
        .env_var_os("DBUS_SESSION_BUS_ADDRESS")
        .is_some_and(|value| !value.is_empty())
    {
        return;
    }

    let Some(runtime_dir) = probe
        .env_var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
    else {
        failures.push(
            "rootless ownership materialization requires user DBus: DBUS_SESSION_BUS_ADDRESS and XDG_RUNTIME_DIR are unset".to_string(),
        );
        return;
    };

    let bus_path = PathBuf::from(runtime_dir).join("bus");
    match probe.path_kind(&bus_path) {
        Ok(PathKind::File) | Ok(PathKind::Socket) => {}
        Ok(PathKind::Missing) => failures.push(format!(
            "rootless ownership materialization requires user DBus socket '{}'",
            bus_path.display()
        )),
        Ok(kind) => failures.push(format!(
            "user DBus path '{}' is not a socket or file ({kind})",
            bus_path.display()
        )),
        Err(error) => failures.push(format!(
            "failed to inspect user DBus path '{}': {error}",
            bus_path.display()
        )),
    }
}

fn check_busctl(probe: &impl PreflightProbe, failures: &mut Vec<String>) {
    match probe.run_busctl_user_status(BUSCTL_TIMEOUT) {
        Ok(output) if output.status.success() => {}
        Ok(output) => failures.push(format!(
            "busctl --user --no-pager status failed with {}{}",
            status_message(output.status),
            command_context(&output)
        )),
        Err(BusctlError::NotFound) => failures.push("busctl not found in PATH".to_string()),
        Err(BusctlError::TimedOut) => failures.push(format!(
            "busctl --user --no-pager status timed out after {}s",
            BUSCTL_TIMEOUT.as_secs()
        )),
        Err(BusctlError::Io(error)) => failures.push(format!(
            "failed to run busctl --user --no-pager status: {error}"
        )),
    }
}

fn status_message(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "signal termination".to_string(),
    }
}

fn command_context(output: &CommandOutput) -> String {
    let context = first_non_empty_line(&output.stderr)
        .or_else(|| first_non_empty_line(&output.stdout))
        .map(|line| format!(": {line}"));
    context.unwrap_or_default()
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

trait PreflightProbe {
    fn path_kind(&self, path: &Path) -> io::Result<PathKind>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn env_var_os(&self, key: &str) -> Option<OsString>;
    fn command_in_path(&self, name: &str) -> io::Result<bool>;
    fn run_busctl_user_status(&self, timeout: Duration) -> Result<CommandOutput, BusctlError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Missing,
    File,
    Directory,
    Socket,
    Other,
}

impl std::fmt::Display for PathKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Missing => "missing",
            Self::File => "file",
            Self::Directory => "directory",
            Self::Socket => "socket",
            Self::Other => "other",
        };
        formatter.write_str(name)
    }
}

#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
enum BusctlError {
    NotFound,
    TimedOut,
    Io(io::Error),
}

struct HostPreflightProbe;

impl PreflightProbe for HostPreflightProbe {
    fn path_kind(&self, path: &Path) -> io::Result<PathKind> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_dir() {
                    Ok(PathKind::Directory)
                } else if file_type.is_file() {
                    Ok(PathKind::File)
                } else if is_socket(&file_type) {
                    Ok(PathKind::Socket)
                } else {
                    Ok(PathKind::Other)
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(PathKind::Missing),
            Err(error) => Err(error),
        }
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn env_var_os(&self, key: &str) -> Option<OsString> {
        env::var_os(key)
    }

    fn command_in_path(&self, name: &str) -> io::Result<bool> {
        let Some(path) = env::var_os("PATH") else {
            return Ok(false);
        };
        for dir in env::split_paths(&path) {
            let candidate = dir.join(name);
            match fs::metadata(&candidate) {
                Ok(metadata)
                    if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 =>
                {
                    return Ok(true);
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    fn run_busctl_user_status(&self, timeout: Duration) -> Result<CommandOutput, BusctlError> {
        run_command_with_timeout(
            OsStr::new("busctl"),
            [
                OsStr::new("--user"),
                OsStr::new("--no-pager"),
                OsStr::new("status"),
            ],
            timeout,
        )
    }
}

#[cfg(unix)]
fn is_socket(file_type: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;

    file_type.is_socket()
}

#[cfg(not(unix))]
fn is_socket(_: &fs::FileType) -> bool {
    false
}

fn run_command_with_timeout<I, S>(
    program: &OsStr,
    args: I,
    timeout: Duration,
) -> Result<CommandOutput, BusctlError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                BusctlError::NotFound
            } else {
                BusctlError::Io(error)
            }
        })?;

    let started = Instant::now();
    loop {
        match child.try_wait().map_err(BusctlError::Io)? {
            Some(_) => {
                let output = child.wait_with_output().map_err(BusctlError::Io)?;
                return Ok(CommandOutput {
                    status: output.status,
                    stdout: output.stdout,
                    stderr: output.stderr,
                });
            }
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BusctlError::TimedOut);
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn libcontainer_preflight_accepts_complete_host_shape() {
        check_libcontainer_ownership_runtime_preflight(&test_idmap(), &FakeProbe::complete())
            .unwrap();
    }

    #[test]
    fn libcontainer_preflight_aggregates_missing_prerequisites() {
        let probe = FakeProbe::default();
        let error =
            check_libcontainer_ownership_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        assert!(matches!(error, RuntimeError::Preflight(_)));
        let message = error.to_string();
        assert!(message.contains("/sys/fs/cgroup"));
        assert!(message.contains("/run/systemd/system"));
        assert!(message.contains("user DBus"));
        assert!(message.contains("busctl not found"));
    }

    #[test]
    fn local_preflight_rejects_disabled_user_namespaces() {
        let mut probe = FakeProbe::complete();
        probe.files.insert(
            "/proc/sys/user/max_user_namespaces".to_string(),
            "0\n".to_string(),
        );
        probe.files.insert(
            "/proc/sys/kernel/unprivileged_userns_clone".to_string(),
            "0\n".to_string(),
        );

        let error = check_local_helper_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("unprivileged user namespaces disabled"));
        assert!(message.contains("unprivileged user namespace clone disabled"));
    }

    #[test]
    fn local_preflight_accepts_userns_and_newidmap_helpers() {
        check_local_helper_runtime_preflight(&test_idmap(), &FakeProbe::complete()).unwrap();
    }

    #[test]
    fn local_preflight_reports_missing_newidmap_helpers() {
        let mut probe = FakeProbe::complete();
        probe.commands.clear();

        let error = check_local_helper_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("newuidmap not found"));
        assert!(message.contains("newgidmap not found"));
    }

    #[test]
    fn preflight_accepts_dbus_session_bus_address_without_runtime_bus_path() {
        let mut probe = FakeProbe::complete();
        probe.envs.remove("XDG_RUNTIME_DIR");
        probe.envs.insert(
            "DBUS_SESSION_BUS_ADDRESS".to_string(),
            OsString::from("unix:path=/run/user/1000/bus"),
        );
        probe.kinds.remove("/run/user/1000/bus");

        check_libcontainer_ownership_runtime_preflight(&test_idmap(), &probe).unwrap();
    }

    #[test]
    fn preflight_rejects_missing_runtime_bus_socket() {
        let mut probe = FakeProbe::complete();
        probe.envs.remove("DBUS_SESSION_BUS_ADDRESS");
        probe.kinds.remove("/run/user/1000/bus");

        let error =
            check_libcontainer_ownership_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        assert!(error.to_string().contains("/run/user/1000/bus"));
    }

    #[test]
    fn preflight_reports_busctl_nonzero_status_with_context() {
        let mut probe = FakeProbe::complete();
        probe.busctl = BusctlResult::Output(CommandOutput {
            status: ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: b"dbus failed\nsecond line\n".to_vec(),
        });

        let error =
            check_libcontainer_ownership_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("exit code 1"));
        assert!(message.contains("dbus failed"));
    }

    #[test]
    fn preflight_reports_busctl_timeout() {
        let mut probe = FakeProbe::complete();
        probe.busctl = BusctlResult::TimedOut;

        let error =
            check_libcontainer_ownership_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn preflight_reports_zero_test_idmap_ranges() {
        let probe = FakeProbe::complete();
        let idmap = MbuildIdmap::for_tests(1000, 1000, 100000, 0, 200000, 0);

        let error = check_local_helper_runtime_preflight(&idmap, &probe).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("empty subuid range"));
        assert!(message.contains("empty subgid range"));
    }

    fn test_idmap() -> MbuildIdmap {
        MbuildIdmap::for_tests(1000, 1000, 100000, 65536, 200000, 65536)
    }

    #[derive(Debug)]
    struct FakeProbe {
        kinds: HashMap<String, PathKind>,
        files: HashMap<String, String>,
        envs: HashMap<String, OsString>,
        commands: HashSet<String>,
        inspect_errors: HashSet<String>,
        busctl: BusctlResult,
    }

    impl FakeProbe {
        fn complete() -> Self {
            let mut probe = Self::default();
            probe
                .kinds
                .insert("/sys/fs/cgroup".to_string(), PathKind::Directory);
            probe.kinds.insert(
                "/sys/fs/cgroup/cgroup.controllers".to_string(),
                PathKind::File,
            );
            probe
                .kinds
                .insert("/run/systemd/system".to_string(), PathKind::Directory);
            probe.kinds.insert(
                "/proc/sys/user/max_user_namespaces".to_string(),
                PathKind::File,
            );
            probe.kinds.insert(
                "/proc/sys/kernel/unprivileged_userns_clone".to_string(),
                PathKind::File,
            );
            probe
                .kinds
                .insert("/run/user/1000/bus".to_string(), PathKind::Socket);
            probe.files.insert(
                "/proc/sys/user/max_user_namespaces".to_string(),
                "1024\n".to_string(),
            );
            probe.files.insert(
                "/proc/sys/kernel/unprivileged_userns_clone".to_string(),
                "1\n".to_string(),
            );
            probe.envs.insert(
                "XDG_RUNTIME_DIR".to_string(),
                OsString::from("/run/user/1000"),
            );
            probe.commands.insert("newuidmap".to_string());
            probe.commands.insert("newgidmap".to_string());
            probe.busctl = BusctlResult::Output(CommandOutput {
                status: ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
            probe
        }
    }

    impl Default for FakeProbe {
        fn default() -> Self {
            Self {
                kinds: HashMap::new(),
                files: HashMap::new(),
                envs: HashMap::new(),
                commands: HashSet::new(),
                inspect_errors: HashSet::new(),
                busctl: BusctlResult::NotFound,
            }
        }
    }

    impl PreflightProbe for FakeProbe {
        fn path_kind(&self, path: &Path) -> io::Result<PathKind> {
            let path = path.display().to_string();
            if self.inspect_errors.contains(&path) {
                return Err(io::Error::other("forced inspect error"));
            }
            Ok(self.kinds.get(&path).copied().unwrap_or(PathKind::Missing))
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            let path = path.display().to_string();
            self.files
                .get(&path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake file"))
        }

        fn env_var_os(&self, key: &str) -> Option<OsString> {
            self.envs.get(key).cloned()
        }

        fn command_in_path(&self, name: &str) -> io::Result<bool> {
            Ok(self.commands.contains(name))
        }

        fn run_busctl_user_status(&self, _: Duration) -> Result<CommandOutput, BusctlError> {
            match &self.busctl {
                BusctlResult::Output(output) => Ok(CommandOutput {
                    status: output.status,
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                }),
                BusctlResult::NotFound => Err(BusctlError::NotFound),
                BusctlResult::TimedOut => Err(BusctlError::TimedOut),
            }
        }
    }

    #[derive(Debug)]
    enum BusctlResult {
        Output(CommandOutput),
        NotFound,
        TimedOut,
    }
}
