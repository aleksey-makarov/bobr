use crate::error::RuntimeError;
use crate::executor::read_executor_error_report;
use crate::idmap::MbuildIdmap;
use mbuild_core::FsTreeManifest;
use mbuild_core::runtime_helper_protocol::{
    HELPER_BINARY_NAME, HELPER_PROTOCOL_VERSION, HelperProtocolInfo,
};
use serde::Serialize;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, OnceLock};
use tracing::warn;
use uuid::Uuid;

struct LocalHelperRun {
    dir: PathBuf,
}

impl Drop for LocalHelperRun {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.dir) {
            warn!(
                "failed to remove local helper workspace '{}': {error}",
                self.dir.display()
            );
        }
    }
}

#[derive(Debug)]
struct LocalHelperTools {
    helper: PathBuf,
    newuidmap: PathBuf,
    newgidmap: PathBuf,
}

pub(crate) fn preflight_local_helper_runtime(idmap: &MbuildIdmap) -> Result<(), RuntimeError> {
    crate::preflight::preflight_local_helper_runtime(idmap)?;
    cached_local_helper_tools()?;
    Ok(())
}

pub(crate) trait LocalHelperOperation {
    type Config: Serialize;

    const COMMAND: &'static str;
    const CONFIG_FILE: &'static str;
    const CONFIG_LABEL: &'static str;

    fn build_config(
        &self,
        run_dir: &Path,
        error_report: &Path,
    ) -> Result<Self::Config, RuntimeError>;
}

pub(crate) fn run_local_helper_operation<O>(
    idmap: &MbuildIdmap,
    workspace: &Path,
    operation: O,
) -> Result<(), RuntimeError>
where
    O: LocalHelperOperation,
{
    run_local_helper_with_config(
        idmap,
        workspace,
        O::COMMAND,
        O::CONFIG_FILE,
        |run_dir, error_report| {
            let config = operation.build_config(run_dir, error_report)?;
            serde_json::to_vec(&config).map_err(|error| {
                RuntimeError::Executor(format!("failed to serialize {}: {error}", O::CONFIG_LABEL))
            })
        },
    )
}

fn run_local_helper_with_config<F>(
    idmap: &MbuildIdmap,
    workspace: &Path,
    operation: &str,
    config_file_name: &str,
    build_config: F,
) -> Result<(), RuntimeError>
where
    F: FnOnce(&Path, &Path) -> Result<Vec<u8>, RuntimeError>,
{
    let tools = cached_local_helper_tools()?;
    let state_root = workspace.join("state");
    let bundles_root = workspace.join("bundles");
    fs::create_dir_all(&state_root)?;
    fs::create_dir_all(&bundles_root)?;

    let dir = bundles_root.join(Uuid::new_v4().simple().to_string());
    fs::create_dir(&dir)?;
    let run = LocalHelperRun {
        dir: fs::canonicalize(dir)?,
    };
    let error_report = run.dir.join("error.json");
    let config_path = run.dir.join(config_file_name);
    fs::File::create(&error_report)?;
    let bytes = build_config(&run.dir, &error_report)?;
    fs::write(&config_path, bytes)?;

    let lifecycle_result = launch_helper(&tools, idmap, operation, &config_path);
    resolve_helper_report(&error_report, lifecycle_result)?;
    Ok(())
}

pub(crate) fn write_helper_manifest(
    path: &Path,
    manifest: &FsTreeManifest,
    label: &str,
) -> Result<(), RuntimeError> {
    let bytes = manifest.to_canonical_bytes().map_err(|error| {
        RuntimeError::InvalidInput(format!("failed to serialize {label}: {error}"))
    })?;
    fs::write(path, bytes)?;
    Ok(())
}

fn launch_helper(
    tools: &LocalHelperTools,
    idmap: &MbuildIdmap,
    operation: &str,
    config_path: &Path,
) -> Result<(), RuntimeError> {
    let child_ready = Pipe::new()?;
    let parent_ready = Pipe::new()?;
    let child_ready_read = child_ready.read_raw();
    let child_ready_write = child_ready.write_raw();
    let parent_ready_read = parent_ready.read_raw();
    let parent_ready_write = parent_ready.write_raw();

    let mut command = Command::new(&tools.helper);
    command
        .arg("wait-exec")
        .arg("--wait-fd")
        .arg(parent_ready_read.to_string())
        .arg("--")
        .arg(operation)
        .arg("--config")
        .arg(config_path)
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
            "failed to spawn runtime helper '{}': {error}",
            tools.helper.display()
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

    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(RuntimeError::Executor(format!(
            "runtime helper operation '{operation}' exited with {}{}",
            status_message(output.status),
            command_context(&output.stderr)
        )))
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
            "failed to signal runtime helper readiness: {}",
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
                "runtime helper closed {label} pipe before signalling readiness"
            )));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(RuntimeError::Executor(format!(
                "failed to read runtime helper {label} pipe: {error}"
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

fn resolve_helper_report(
    path: &Path,
    lifecycle_result: Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    match read_executor_error_report(path) {
        Ok(Some(report)) => Err(RuntimeError::Executor(report.to_string())),
        Ok(None) => lifecycle_result,
        Err(RuntimeError::Executor(message)) => match lifecycle_result {
            Ok(()) => Err(RuntimeError::Executor(message)),
            Err(lifecycle_error) => Err(RuntimeError::Executor(format!(
                "{message}; lifecycle error was: {lifecycle_error}"
            ))),
        },
        Err(error) => Err(error),
    }
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn cached_local_helper_tools() -> Result<Arc<LocalHelperTools>, RuntimeError> {
    static TOOLS: OnceLock<Result<Arc<LocalHelperTools>, String>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            resolve_and_preflight_tools()
                .map(Arc::new)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|message| RuntimeError::Preflight(message.clone()))
}

fn resolve_and_preflight_tools() -> Result<LocalHelperTools, RuntimeError> {
    let helper = resolve_helper_path()?;
    require_executable_file(&helper, "runtime helper")?;
    preflight_helper_protocol(&helper)?;
    let newuidmap = resolve_path_program(OsStr::new("newuidmap"))?;
    let newgidmap = resolve_path_program(OsStr::new("newgidmap"))?;
    require_executable_file(&newuidmap, "newuidmap")?;
    require_executable_file(&newgidmap, "newgidmap")?;
    Ok(LocalHelperTools {
        helper,
        newuidmap,
        newgidmap,
    })
}

fn preflight_helper_protocol(path: &Path) -> Result<(), RuntimeError> {
    let output = Command::new(path)
        .arg("--protocol-info")
        .output()
        .map_err(|error| {
            RuntimeError::Preflight(format!(
                "failed to run runtime helper preflight '{} --protocol-info': {error}",
                path.display()
            ))
        })?;
    if !output.status.success() {
        return Err(RuntimeError::Preflight(format!(
            "runtime helper preflight '{} --protocol-info' failed with {}{}",
            path.display(),
            status_message(output.status),
            command_context(&output.stderr)
        )));
    }
    let info = serde_json::from_slice::<HelperProtocolInfo>(&output.stdout).map_err(|error| {
        RuntimeError::Preflight(format!(
            "failed to parse runtime helper protocol info from '{}': {error}",
            path.display()
        ))
    })?;
    if info.name != HELPER_BINARY_NAME || info.protocol_version != HELPER_PROTOCOL_VERSION {
        return Err(RuntimeError::Preflight(format!(
            "runtime helper '{}' has incompatible protocol {:?}; expected name '{}' protocol {}",
            path.display(),
            info,
            HELPER_BINARY_NAME,
            HELPER_PROTOCOL_VERSION
        )));
    }
    Ok(())
}

fn resolve_helper_path() -> Result<PathBuf, RuntimeError> {
    resolve_helper_path_from(
        env::var_os("MBUILD_RUNTIME_HELPER").map(PathBuf::from),
        env::current_exe().ok().as_deref(),
        env::var_os("PATH"),
    )
}

fn resolve_helper_path_from(
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
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join(HELPER_BINARY_NAME);
            checked.push(sibling.clone());
            if sibling.exists() {
                return Ok(sibling);
            }
            if parent.file_name().and_then(|name| name.to_str()) == Some("deps")
                && let Some(profile_dir) = parent.parent()
            {
                let candidate = profile_dir.join(HELPER_BINARY_NAME);
                checked.push(candidate.clone());
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
        for ancestor in current_exe.ancestors() {
            let target_dir = ancestor.join("target");
            for profile in ["debug", "release"] {
                let candidate = target_dir.join(profile).join(HELPER_BINARY_NAME);
                checked.push(candidate.clone());
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    if let Some(path) = path_env {
        for dir in env::split_paths(&path) {
            let candidate = dir.join(HELPER_BINARY_NAME);
            checked.push(candidate.clone());
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Err(RuntimeError::Preflight(format!(
        "failed to find runtime helper '{}'; set MBUILD_RUNTIME_HELPER or place it next to the current executable; checked {}",
        HELPER_BINARY_NAME,
        checked
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
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

fn status_message(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "signal termination".to_string(),
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

    #[test]
    fn resolve_helper_uses_env_override_first() {
        let got = resolve_helper_path_from(
            Some(PathBuf::from("/bin/sh")),
            Some(Path::new("/tmp/target/debug/test-bin")),
            None,
        )
        .unwrap();

        assert_eq!(got, PathBuf::from("/bin/sh"));
    }
}
