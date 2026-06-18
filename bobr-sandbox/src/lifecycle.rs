use crate::reports::{
    command_context, read_sandbox_failure_report, read_sandbox_success_steps, status_message,
};
use bobr_runtime::runtime::RuntimeError;
use mbuild_sandbox_runner_core::{SandboxStepReport, write_handshake_byte};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

const RUNNER_WAIT_FD: RawFd = 3;

pub(crate) fn run_sandbox_runner(
    runner_path: &Path,
    launcher_config: &Path,
    success_report: &Path,
    failure_report: &Path,
) -> Result<Vec<SandboxStepReport>, RuntimeError> {
    let wait_pipe = Pipe::new().map_err(|error| {
        RuntimeError::new(format!(
            "failed to create sandbox runner wait pipe: {error}"
        ))
    })?;
    let Pipe {
        read: wait_read,
        write: wait_write,
    } = wait_pipe;
    let wait_read_fd = wait_read.as_raw_fd();
    let mut command = Command::new(runner_path);
    command
        .arg("launch")
        .arg("--wait-fd")
        .arg(RUNNER_WAIT_FD.to_string())
        .arg("--config")
        .arg(launcher_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // `pre_exec` runs after fork and before exec. Keep it to async-signal-safe
    // fd operations only: no allocation, formatting, locks, or Rust runtime
    // callbacks. The launcher needs exactly one inherited fd for the start
    // handshake; all other transport state stays in the parent.
    unsafe {
        command.pre_exec(move || prepare_runner_wait_fd(wait_read_fd));
    }

    let child = command.spawn().map_err(|error| {
        RuntimeError::new(format!(
            "failed to spawn sandbox runner '{}': {error}",
            runner_path.display()
        ))
    })?;
    drop(wait_read);

    if let Err(error) = write_handshake_byte(wait_write.as_raw_fd()) {
        let stderr = terminate_child_with_output(child).unwrap_or_default();
        return Err(RuntimeError::new(format!(
            "failed to signal sandbox runner readiness: {error}{}",
            command_context(&stderr)
        )));
    }
    drop(wait_write);

    let output = child.wait_with_output().map_err(|error| {
        RuntimeError::new(format!(
            "failed to wait for sandbox runner '{}': {error}",
            runner_path.display()
        ))
    })?;
    if output.status.success() {
        read_sandbox_success_steps(success_report)
    } else {
        Err(read_sandbox_failure_report(
            failure_report,
            format!(
                "sandbox runner exited with {}{}",
                status_message(output.status),
                command_context(&output.stderr)
            ),
        ))
    }
}

fn terminate_child_with_output(mut child: Child) -> io::Result<Vec<u8>> {
    let _ = child.kill();
    child.wait_with_output().map(|output| output.stderr)
}

fn prepare_runner_wait_fd(fd: RawFd) -> io::Result<()> {
    duplicate_runner_wait_fd(fd, RUNNER_WAIT_FD)
}

fn duplicate_runner_wait_fd(fd: RawFd, target_fd: RawFd) -> io::Result<()> {
    if fd == target_fd {
        clear_cloexec(fd)
    } else if unsafe { libc::dup2(fd, target_fd) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Pipe {
    fn new() -> io::Result<Self> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read: unsafe { OwnedFd::from_raw_fd(fds[0]) },
            write: unsafe { OwnedFd::from_raw_fd(fds[1]) },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd_flags(fd: RawFd) -> i32 {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed: {}", io::Error::last_os_error());
        flags
    }

    fn cloexec_is_set(fd: RawFd) -> bool {
        fd_flags(fd) & libc::FD_CLOEXEC != 0
    }

    #[test]
    fn pipe_new_sets_cloexec_on_both_ends() {
        let pipe = Pipe::new().unwrap();

        assert!(cloexec_is_set(pipe.read.as_raw_fd()));
        assert!(cloexec_is_set(pipe.write.as_raw_fd()));
    }

    #[test]
    fn prepare_runner_wait_fd_duplicates_without_cloexec() {
        let source = Pipe::new().unwrap();
        let target = Pipe::new().unwrap();
        let target_fd = target.write.as_raw_fd();

        duplicate_runner_wait_fd(source.read.as_raw_fd(), target_fd).unwrap();

        assert!(!cloexec_is_set(target_fd));
    }

    #[test]
    fn prepare_runner_wait_fd_clears_cloexec_when_fd_already_matches() {
        let pipe = Pipe::new().unwrap();
        let fd = pipe.read.as_raw_fd();
        assert!(cloexec_is_set(fd));

        duplicate_runner_wait_fd(fd, fd).unwrap();

        assert!(!cloexec_is_set(fd));
    }

    #[test]
    fn child_wait_with_output_drains_large_stderr() {
        let child = Command::new("sh")
            .arg("-c")
            .arg("dd if=/dev/zero bs=8192 count=128 1>&2 2>/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let output = child.wait_with_output().unwrap();

        assert!(output.status.success());
        assert_eq!(output.stderr.len(), 1024 * 1024);
    }
}
