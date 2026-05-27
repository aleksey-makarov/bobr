use super::SandboxBuildOutcome;
use super::mounts::PreparedSandbox;
use super::reports::{
    add_stderr_context, command_context, read_sandbox_failure_report, read_sandbox_success_report,
    status_message,
};
use super::tools::SandboxTools;
use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use tracing::warn;

pub(super) struct SandboxLifecycle {
    child: Option<LauncherChild>,
    run_dir: PathBuf,
    success_report_path: PathBuf,
    failure_report_path: PathBuf,
    cleaned_up: bool,
}

impl SandboxLifecycle {
    pub(super) fn start(
        tools: &SandboxTools,
        idmap: &MbuildIdmap,
        prepared: PreparedSandbox,
    ) -> Result<Self, RuntimeError> {
        let (child, handshake) =
            fork_sandbox_runner(&tools.runner.host_path, &prepared.launcher_config)?;

        if let Err(error) = complete_launcher_setup(&child, handshake, tools, idmap) {
            let stderr = child.terminate_with_output().unwrap_or_default();
            return Err(add_stderr_context(error, &stderr));
        }

        Ok(Self {
            child: Some(child),
            run_dir: prepared.dirs.root,
            success_report_path: prepared.runtime_files.success_report,
            failure_report_path: prepared.runtime_files.failure_report,
            cleaned_up: false,
        })
    }

    pub(super) fn wait_for_outcome(&mut self) -> Result<SandboxBuildOutcome, RuntimeError> {
        let child = self
            .child
            .take()
            .ok_or_else(|| RuntimeError::Executor("sandbox runner already waited".to_string()))?;
        let output = child.wait_with_output()?;
        if output.success {
            read_sandbox_success_report(&self.success_report_path)
        } else {
            Err(read_sandbox_failure_report(
                &self.failure_report_path,
                format!(
                    "sandbox runner exited with {}{}",
                    output.status_message,
                    command_context(&output.stderr)
                ),
            ))
        }
    }

    pub(super) fn cleanup(&mut self) -> Result<(), RuntimeError> {
        if self.cleaned_up {
            return Ok(());
        }
        if let Some(child) = self.child.take() {
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
        if let Some(child) = self.child.take() {
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

struct LauncherChild {
    pid: libc::pid_t,
    stderr: OwnedFd,
}

struct LauncherOutput {
    success: bool,
    status_message: String,
    stderr: Vec<u8>,
}

struct LauncherHandshake {
    userns_ready_read: OwnedFd,
    idmap_ready_write: OwnedFd,
    runner_start_write: OwnedFd,
}

impl LauncherChild {
    fn pid_u32(&self) -> u32 {
        self.pid as u32
    }

    fn wait_with_output(self) -> Result<LauncherOutput, RuntimeError> {
        let stderr = self.stderr;
        let stderr_reader = thread::spawn(move || {
            let mut stderr_bytes = Vec::new();
            let mut file = File::from(stderr);
            file.read_to_end(&mut stderr_bytes)?;
            Ok::<_, io::Error>(stderr_bytes)
        });
        let status = wait_for_pid(self.pid)?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| RuntimeError::Executor("sandbox stderr reader panicked".to_string()))??;
        Ok(LauncherOutput {
            success: raw_wait_status_success(status),
            status_message: raw_wait_status_message(status),
            stderr,
        })
    }

    fn terminate_with_output(self) -> Result<Vec<u8>, RuntimeError> {
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        self.wait_with_output().map(|output| output.stderr)
    }
}

fn fork_sandbox_runner(
    runner_path: &Path,
    launcher_config: &Path,
) -> Result<(LauncherChild, LauncherHandshake), RuntimeError> {
    let stderr = Pipe::new()?;
    let userns_ready = Pipe::new()?;
    let idmap_ready = Pipe::new()?;
    let runner_start = Pipe::new()?;
    let userns_ready_write = userns_ready.write_raw();
    let idmap_ready_read = idmap_ready.read_raw();
    let runner_start_read = runner_start.read_raw();
    let c_runner = path_cstring(runner_path)?;
    let arg0 = c_runner.clone();
    let args = [
        arg0,
        CString::new("launch").unwrap(),
        CString::new("--wait-fd").unwrap(),
        CString::new(runner_start_read.to_string()).unwrap(),
        CString::new("--config").unwrap(),
        path_cstring(launcher_config)?,
    ];
    let mut arg_ptrs = args.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    arg_ptrs.push(std::ptr::null());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(RuntimeError::Executor(format!(
            "failed to fork sandbox runner '{}': {}",
            runner_path.display(),
            io::Error::last_os_error()
        )));
    }
    if pid == 0 {
        child_exec_sandbox_runner(
            &c_runner,
            &arg_ptrs,
            stderr.write_raw(),
            userns_ready_write,
            idmap_ready_read,
        );
    }

    let Pipe {
        read: stderr_read,
        write: stderr_write,
    } = stderr;
    let Pipe {
        read: userns_ready_read,
        write: userns_ready_write,
    } = userns_ready;
    let Pipe {
        read: idmap_ready_read,
        write: idmap_ready_write,
    } = idmap_ready;
    let Pipe {
        read: runner_start_read,
        write: runner_start_write,
    } = runner_start;
    drop(stderr_write);
    drop(userns_ready_write);
    drop(idmap_ready_read);
    drop(runner_start_read);
    Ok((
        LauncherChild {
            pid,
            stderr: stderr_read,
        },
        LauncherHandshake {
            userns_ready_read,
            idmap_ready_write,
            runner_start_write,
        },
    ))
}

fn complete_launcher_setup(
    child: &LauncherChild,
    handshake: LauncherHandshake,
    tools: &SandboxTools,
    idmap: &MbuildIdmap,
) -> Result<(), RuntimeError> {
    wait_for_child_userns(handshake.userns_ready_read.as_raw_fd())?;
    configure_id_maps(&tools.newuidmap, &tools.newgidmap, child.pid_u32(), idmap)?;
    signal_child_ready(handshake.idmap_ready_write.as_raw_fd())?;
    signal_child_ready(handshake.runner_start_write.as_raw_fd())
}

fn child_exec_sandbox_runner(
    runner: &CString,
    args: &[*const libc::c_char],
    stderr_write: RawFd,
    child_ready_write: RawFd,
    exec_ready_read: RawFd,
) -> ! {
    if child_setup_stdio(stderr_write).is_err() {
        unsafe { libc::_exit(127) };
    }
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        child_write_stderr(b"failed to unshare user namespace\n");
        unsafe { libc::_exit(127) };
    }
    let byte = [1_u8; 1];
    let written = unsafe { libc::write(child_ready_write, byte.as_ptr().cast(), byte.len()) };
    if written != 1 {
        child_write_stderr(b"failed to signal user namespace readiness\n");
        unsafe { libc::_exit(127) };
    }
    if pre_exec_wait_one_byte(exec_ready_read).is_err() {
        child_write_stderr(b"failed to wait for exec readiness\n");
        unsafe { libc::_exit(127) };
    }
    unsafe {
        libc::close(child_ready_write);
        libc::close(exec_ready_read);
        libc::execv(runner.as_ptr(), args.as_ptr());
    }
    child_write_stderr(b"failed to exec sandbox runner\n");
    unsafe { libc::_exit(127) };
}

fn child_setup_stdio(stderr_write: RawFd) -> io::Result<()> {
    let dev_null = c"/dev/null";
    let null_fd = unsafe { libc::open(dev_null.as_ptr(), libc::O_RDWR) };
    if null_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::dup2(null_fd, libc::STDIN_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::dup2(null_fd, libc::STDOUT_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::dup2(stderr_write, libc::STDERR_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::close(null_fd);
        libc::close(stderr_write);
    }
    Ok(())
}

fn child_write_stderr(message: &'static [u8]) {
    unsafe {
        let _ = libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
    }
}

fn wait_for_pid(pid: libc::pid_t) -> Result<i32, RuntimeError> {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(status);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(RuntimeError::Executor(format!(
                "failed to wait for sandbox runner pid {pid}: {error}"
            )));
        }
    }
}

fn raw_wait_status_success(status: i32) -> bool {
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

fn raw_wait_status_message(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exit code {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        format!("raw wait status {status}")
    }
}

fn path_cstring(path: &Path) -> Result<CString, RuntimeError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        RuntimeError::InvalidInput(format!("path contains NUL byte: '{}'", path.display()))
    })
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

fn pre_exec_wait_one_byte(fd: RawFd) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    loop {
        let result = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if result == 1 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "parent closed sandbox exec readiness pipe",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
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

fn terminate_child(child: LauncherChild) {
    unsafe {
        libc::kill(child.pid, libc::SIGKILL);
    }
    let _ = wait_for_pid(child.pid);
}

fn remove_run_dir(path: &Path) -> Result<(), RuntimeError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::Io(error)),
    }
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
    fn launcher_child_wait_with_output_drains_large_stderr() {
        let stderr = Pipe::new().unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());
        if pid == 0 {
            let Pipe { read, write } = stderr;
            drop(read);
            let mut remaining = 1024 * 1024;
            let chunk = [b'x'; 8192];
            while remaining > 0 {
                let size = remaining.min(chunk.len());
                let written =
                    unsafe { libc::write(write.as_raw_fd(), chunk.as_ptr().cast(), size) };
                if written <= 0 {
                    unsafe { libc::_exit(2) };
                }
                remaining -= written as usize;
            }
            unsafe { libc::_exit(0) };
        }
        let Pipe { read, write } = stderr;
        drop(write);
        let child = LauncherChild { pid, stderr: read };

        let output = child.wait_with_output().unwrap();

        assert!(output.success);
        assert_eq!(output.stderr.len(), 1024 * 1024);
    }
}
