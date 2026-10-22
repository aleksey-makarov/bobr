use crate::reports::{command_context, read_sandbox_failure_report, status_message};
use bobr_runtime::runtime::{RuntimeError, RuntimeResult};
use bobr_sandbox_launcher::{SandboxEvent, SandboxEventDecoder, write_handshake_byte};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};

const LAUNCHER_WAIT_FD: RawFd = 3;
const LAUNCHER_EVENT_FD: RawFd = 4;
const FIRST_TEMPORARY_FD: RawFd = 5;
const PROCESS_POLL_INTERVAL_MS: i32 = 100;

#[derive(Debug)]
pub(crate) enum LauncherOutcome {
    Completed,
    Cancelled,
}

pub(crate) struct CancellationReceiver {
    fifo: File,
}

impl CancellationReceiver {
    pub(crate) fn open(path: &Path) -> Result<Self, RuntimeError> {
        let fifo = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| {
                RuntimeError::new(format!(
                    "failed to open sandbox cancellation FIFO '{}': {error}",
                    path.display()
                ))
            })?;
        let metadata = fifo.metadata().map_err(|error| {
            RuntimeError::new(format!(
                "failed to inspect sandbox cancellation FIFO '{}': {error}",
                path.display()
            ))
        })?;
        if !metadata.file_type().is_fifo() {
            return Err(RuntimeError::new(format!(
                "sandbox cancellation path is not a FIFO: '{}'",
                path.display()
            )));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(RuntimeError::new(format!(
                "sandbox cancellation FIFO '{}' is not owned by the runtime user",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(RuntimeError::new(format!(
                "sandbox cancellation FIFO '{}' is accessible by other users",
                path.display()
            )));
        }
        Ok(Self { fifo })
    }

    pub(crate) fn is_cancelled(&self) -> Result<bool, RuntimeError> {
        cancellation_eof(self.fifo.as_raw_fd()).map_err(|error| {
            RuntimeError::new(format!("failed to read sandbox cancellation FIFO: {error}"))
        })
    }
}

pub(crate) fn run_sandbox_launcher(
    launcher_path: &Path,
    launcher_config: &Path,
    failure_report: &Path,
    cancellation: &CancellationReceiver,
    emit: &mut dyn FnMut(SandboxEvent) -> RuntimeResult<()>,
) -> Result<LauncherOutcome, RuntimeError> {
    if cancellation.is_cancelled()? {
        return Ok(LauncherOutcome::Cancelled);
    }
    let wait_pipe = Pipe::new().map_err(|error| {
        RuntimeError::new(format!(
            "failed to create sandbox launcher wait pipe: {error}"
        ))
    })?;
    let Pipe {
        read: wait_read,
        write: wait_write,
    } = wait_pipe;
    let event_pipe = Pipe::new().map_err(|error| {
        RuntimeError::new(format!("failed to create sandbox event pipe: {error}"))
    })?;
    let Pipe {
        read: event_read,
        write: event_write,
    } = event_pipe;
    let inherited_wait =
        duplicate_cloexec(wait_read.as_raw_fd(), FIRST_TEMPORARY_FD).map_err(|error| {
            RuntimeError::new(format!("failed to duplicate launcher wait fd: {error}"))
        })?;
    let inherited_event =
        duplicate_cloexec(event_write.as_raw_fd(), FIRST_TEMPORARY_FD).map_err(|error| {
            RuntimeError::new(format!("failed to duplicate launcher event fd: {error}"))
        })?;
    let inherited_wait_fd = inherited_wait.as_raw_fd();
    let inherited_event_fd = inherited_event.as_raw_fd();
    let expected_parent = unsafe { libc::getpid() };
    let mut command = Command::new(launcher_path);
    command
        .arg("launch")
        .arg("--wait-fd")
        .arg(LAUNCHER_WAIT_FD.to_string())
        .arg("--event-fd")
        .arg(LAUNCHER_EVENT_FD.to_string())
        .arg("--config")
        .arg(launcher_config)
        // Keep terminal signals in the Bobr process group. This matters for
        // HostRuntime, where the runtime function itself has no worker process
        // boundary: Ctrl-C must reach the cancellation FIFO rather than kill
        // the launcher and turn an expected cancellation into a build failure.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // `pre_exec` runs after fork and before exec. Keep it to async-signal-safe
    // fd operations only: no allocation, formatting, locks, or Rust runtime
    // callbacks. The launcher needs exactly one inherited fd for the start
    // handshake; all other transport state stays in the parent.
    unsafe {
        command.pre_exec(move || {
            duplicate_launcher_fd(inherited_wait_fd, LAUNCHER_WAIT_FD)?;
            duplicate_launcher_fd(inherited_event_fd, LAUNCHER_EVENT_FD)?;
            set_parent_death_signal(expected_parent)
        });
    }

    let mut child = LauncherChild::new(command.spawn().map_err(|error| {
        RuntimeError::new(format!(
            "failed to spawn sandbox launcher '{}': {error}",
            launcher_path.display()
        ))
    })?);
    drop(wait_read);
    drop(event_write);
    drop(inherited_wait);
    drop(inherited_event);

    if cancellation.is_cancelled()? {
        let _ = terminate_child(&mut child);
        return Ok(LauncherOutcome::Cancelled);
    }

    if let Err(error) = write_handshake_byte(wait_write.as_raw_fd()) {
        let cancelled = cancellation.is_cancelled()?;
        let stderr = terminate_child(&mut child).unwrap_or_default();
        if cancelled {
            return Ok(LauncherOutcome::Cancelled);
        }
        return Err(RuntimeError::new(format!(
            "failed to signal sandbox launcher readiness: {error}{}",
            command_context(&stderr)
        )));
    }
    drop(wait_write);

    let mut stderr = child.child.stderr.take().ok_or_else(|| {
        RuntimeError::new("sandbox launcher stderr pipe was not created".to_string())
    })?;
    set_nonblocking(stderr.as_raw_fd()).map_err(|error| {
        RuntimeError::new(format!(
            "failed to make sandbox launcher stderr nonblocking: {error}"
        ))
    })?;
    let mut stderr_bytes = Vec::new();
    let mut event_reader = File::from(event_read);
    set_nonblocking(event_reader.as_raw_fd()).map_err(|error| {
        RuntimeError::new(format!(
            "failed to make sandbox event pipe nonblocking: {error}"
        ))
    })?;
    let mut event_decoder = SandboxEventDecoder::default();
    let status = loop {
        drain_stderr(&mut stderr, &mut stderr_bytes).map_err(|error| {
            RuntimeError::new(format!("failed to read sandbox launcher stderr: {error}"))
        })?;
        drain_sandbox_events(&mut event_reader, &mut event_decoder, emit)?;
        if let Some(status) = child.try_wait().map_err(|error| {
            RuntimeError::new(format!(
                "failed to wait for sandbox launcher '{}': {error}",
                launcher_path.display()
            ))
        })? {
            break status;
        }
        if poll_launcher(
            cancellation.fifo.as_raw_fd(),
            stderr.as_raw_fd(),
            event_reader.as_raw_fd(),
        )
        .map_err(|error| {
            RuntimeError::new(format!(
                "failed while waiting for sandbox launcher: {error}"
            ))
        })? {
            let _ = child.kill_and_reap();
            let _ = drain_stderr(&mut stderr, &mut stderr_bytes);
            let _ = drain_sandbox_events(&mut event_reader, &mut event_decoder, emit);
            return Ok(LauncherOutcome::Cancelled);
        }
    };
    let _ = drain_stderr(&mut stderr, &mut stderr_bytes);
    let event_eof = drain_sandbox_events(&mut event_reader, &mut event_decoder, emit)?;
    if !event_eof {
        return Err(RuntimeError::new(
            "sandbox event pipe remained open after launcher exit",
        ));
    }
    event_decoder
        .finish()
        .map_err(|error| RuntimeError::new(error.to_string()))?;

    if status.success() {
        Ok(LauncherOutcome::Completed)
    } else {
        Err(read_sandbox_failure_report(
            failure_report,
            format!(
                "sandbox launcher exited with {}{}",
                status_message(status),
                command_context(&stderr_bytes)
            ),
        ))
    }
}

fn terminate_child(child: &mut LauncherChild) -> io::Result<Vec<u8>> {
    child.kill_and_reap()?;
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.child.stderr.take() {
        pipe.read_to_end(&mut stderr)?;
    }
    Ok(stderr)
}

struct LauncherChild {
    child: Child,
    reaped: bool,
}

impl LauncherChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn kill_and_reap(&mut self) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        let _ = self.child.kill();
        self.child.wait()?;
        self.reaped = true;
        Ok(())
    }
}

impl Drop for LauncherChild {
    fn drop(&mut self) {
        let _ = self.kill_and_reap();
    }
}

fn set_parent_death_signal(expected_parent: libc::pid_t) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::getppid() } != expected_parent {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_stderr(stderr: &mut ChildStderr, bytes: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn drain_sandbox_events(
    reader: &mut File,
    decoder: &mut SandboxEventDecoder,
    emit: &mut dyn FnMut(SandboxEvent) -> RuntimeResult<()>,
) -> RuntimeResult<bool> {
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                decoder.push(&buffer[..count]);
                while let Some(event) = decoder
                    .next_event()
                    .map_err(|error| RuntimeError::new(error.to_string()))?
                {
                    emit(event)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(RuntimeError::new(error.to_string())),
        }
    }
}

fn cancellation_eof(fd: RawFd) -> io::Result<bool> {
    let mut byte = 0_u8;
    let result = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
    if result == 0 {
        return Ok(true);
    }
    if result > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cancellation FIFO unexpectedly contained data",
        ));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else if error.kind() == io::ErrorKind::Interrupted {
        cancellation_eof(fd)
    } else {
        Err(error)
    }
}

fn poll_launcher(cancellation_fd: RawFd, stderr_fd: RawFd, event_fd: RawFd) -> io::Result<bool> {
    let mut pollfds = [
        libc::pollfd {
            fd: cancellation_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
        libc::pollfd {
            fd: event_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
    ];
    let result = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            PROCESS_POLL_INTERVAL_MS,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(pollfds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0)
}

fn duplicate_cloexec(fd: RawFd, minimum: RawFd) -> io::Result<OwnedFd> {
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
    if duplicated < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }
}

fn duplicate_launcher_fd(fd: RawFd, target_fd: RawFd) -> io::Result<()> {
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
    use bobr_core::CancellationToken;
    use std::ffi::CString;
    use std::mem::size_of;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    fn fd_flags(fd: RawFd) -> i32 {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed: {}", io::Error::last_os_error());
        flags
    }

    fn cloexec_is_set(fd: RawFd) -> bool {
        fd_flags(fd) & libc::FD_CLOEXEC != 0
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn make_fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    fn wait_for_byte(file: &File) {
        let mut pollfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut pollfd, 1, -1) }, 1);
        let mut byte = [0_u8; 1];
        assert_eq!(
            unsafe { libc::read(file.as_raw_fd(), byte.as_mut_ptr().cast(), 1) },
            1
        );
    }

    #[test]
    fn pipe_new_sets_cloexec_on_both_ends() {
        let pipe = Pipe::new().unwrap();

        assert!(cloexec_is_set(pipe.read.as_raw_fd()));
        assert!(cloexec_is_set(pipe.write.as_raw_fd()));
    }

    #[test]
    fn launcher_fd_is_duplicated_without_cloexec() {
        let source = Pipe::new().unwrap();
        let target = Pipe::new().unwrap();
        let target_fd = target.write.as_raw_fd();

        duplicate_launcher_fd(source.read.as_raw_fd(), target_fd).unwrap();

        assert!(!cloexec_is_set(target_fd));
    }

    #[test]
    fn launcher_fd_clears_cloexec_when_already_at_target() {
        let pipe = Pipe::new().unwrap();
        let fd = pipe.read.as_raw_fd();
        assert!(cloexec_is_set(fd));

        duplicate_launcher_fd(fd, fd).unwrap();

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

    #[test]
    fn cancellation_receiver_rejects_regular_files_and_symlinks() {
        let temp = tempdir().unwrap();
        let regular = temp.path().join("regular");
        std::fs::write(&regular, []).unwrap();
        let symlink = temp.path().join("symlink");
        std::os::unix::fs::symlink(&regular, &symlink).unwrap();

        assert!(CancellationReceiver::open(&regular).is_err());
        assert!(CancellationReceiver::open(&symlink).is_err());
    }

    #[test]
    fn cancellation_receiver_rejects_public_fifo() {
        let temp = tempdir().unwrap();
        let fifo = temp.path().join("cancel");
        make_fifo(&fifo);
        std::fs::set_permissions(&fifo, std::fs::Permissions::from_mode(0o666)).unwrap();

        assert!(CancellationReceiver::open(&fifo).is_err());
    }

    #[test]
    fn cancellation_receiver_is_close_on_exec_and_observes_late_open() {
        let token = CancellationToken::new().unwrap();
        token.cancel();
        let receiver = CancellationReceiver::open(token.fifo_path()).unwrap();

        assert!(cloexec_is_set(receiver.fifo.as_raw_fd()));
        assert!(receiver.is_cancelled().unwrap());
    }

    #[test]
    fn launcher_has_its_own_process_group_and_cancellation_reaps_it() {
        let temp = tempdir().unwrap();
        let launcher = temp.path().join("launcher.sh");
        write_script(
            &launcher,
            "dd bs=1 count=1 <&3 >/dev/null 2>/dev/null\nprintf '%s' \"$$\" > \"$7/pid\"\nprintf x > \"$7/ready\"\nexec sleep 60",
        );
        let launcher_config = temp.path().join("config");
        std::fs::create_dir(&launcher_config).unwrap();
        let ready = launcher_config.join("ready");
        make_fifo(&ready);
        let ready_reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&ready)
            .unwrap();
        let token = CancellationToken::new().unwrap();
        let thread_token = token.clone();
        let launcher = Arc::new(launcher);
        let thread_launcher = launcher.clone();
        let failure = temp.path().join("failure.json");
        let thread_failure = failure.clone();
        let thread_launcher_config = launcher_config.clone();
        let thread = thread::spawn(move || {
            let receiver = CancellationReceiver::open(thread_token.fifo_path()).unwrap();
            let mut emit = |_| Ok(());
            run_sandbox_launcher(
                &thread_launcher,
                &thread_launcher_config,
                &thread_failure,
                &receiver,
                &mut emit,
            )
            .unwrap()
        });

        wait_for_byte(&ready_reader);
        let launcher_pid = std::fs::read_to_string(launcher_config.join("pid"))
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert_eq!(unsafe { libc::getpgid(launcher_pid) }, launcher_pid);
        assert_ne!(unsafe { libc::getpgrp() }, launcher_pid);
        token.cancel();

        assert!(matches!(thread.join().unwrap(), LauncherOutcome::Cancelled));
    }

    #[test]
    fn launcher_wait_drains_large_stderr() {
        let temp = tempdir().unwrap();
        let launcher = temp.path().join("launcher.sh");
        write_script(
            &launcher,
            "dd bs=1 count=1 <&3 >/dev/null 2>/dev/null\ndd if=/dev/zero bs=8192 count=128 1>&2 2>/dev/null\nexit 7",
        );
        let token = CancellationToken::new().unwrap();
        let receiver = CancellationReceiver::open(token.fifo_path()).unwrap();
        let mut emit = |_| Ok(());
        let error = run_sandbox_launcher(
            &launcher,
            &temp.path().join("config"),
            &temp.path().join("failure.json"),
            &receiver,
            &mut emit,
        )
        .unwrap_err();

        assert!(error.to_string().contains("sandbox launcher exited"));
    }

    #[test]
    fn launcher_wait_delivers_last_event_before_success() {
        let temp = tempdir().unwrap();
        let launcher = temp.path().join("launcher.sh");
        write_script(
            &launcher,
            "dd bs=1 count=1 <&3 >/dev/null 2>/dev/null\ncat \"$7/events.bin\" >&4",
        );
        let launcher_config = temp.path().join("config");
        std::fs::create_dir(&launcher_config).unwrap();
        let expected = SandboxEvent::StepStarted {
            index: 1,
            total: 3,
            name: "build".to_string(),
            run_as: bobr_sandbox_launcher::RunnerRunAs::BuildUser,
        };
        let mut frame = Vec::new();
        bobr_sandbox_launcher::write_sandbox_event(&mut frame, &expected).unwrap();
        std::fs::write(launcher_config.join("events.bin"), frame).unwrap();
        let token = CancellationToken::new().unwrap();
        let receiver = CancellationReceiver::open(token.fifo_path()).unwrap();
        let mut events = Vec::new();

        let outcome = run_sandbox_launcher(
            &launcher,
            &launcher_config,
            &temp.path().join("failure.json"),
            &receiver,
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .unwrap();

        assert!(matches!(outcome, LauncherOutcome::Completed));
        assert_eq!(events, vec![expected]);
    }

    #[test]
    fn parent_death_signal_kills_launcher_process() {
        let ready = Pipe::new().unwrap();
        let supervisor = unsafe { libc::fork() };
        assert!(
            supervisor >= 0,
            "fork failed: {}",
            io::Error::last_os_error()
        );
        if supervisor == 0 {
            let launcher = unsafe { libc::fork() };
            if launcher < 0 {
                unsafe { libc::_exit(2) };
            }
            if launcher == 0 {
                let parent = unsafe { libc::getppid() };
                if set_parent_death_signal(parent).is_err() {
                    unsafe { libc::_exit(3) };
                }
                let pid = unsafe { libc::getpid() };
                let written = unsafe {
                    libc::write(
                        ready.write.as_raw_fd(),
                        (&pid as *const libc::pid_t).cast(),
                        size_of::<libc::pid_t>(),
                    )
                };
                if written != size_of::<libc::pid_t>() as isize {
                    unsafe { libc::_exit(4) };
                }
                loop {
                    unsafe { libc::pause() };
                }
            }
            loop {
                unsafe { libc::pause() };
            }
        }

        drop(ready.write);
        let mut launcher = 0 as libc::pid_t;
        let read = unsafe {
            libc::read(
                ready.read.as_raw_fd(),
                (&mut launcher as *mut libc::pid_t).cast(),
                size_of::<libc::pid_t>(),
            )
        };
        assert_eq!(read, size_of::<libc::pid_t>() as isize);
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, launcher, 0) as RawFd };
        assert!(
            pidfd >= 0,
            "pidfd_open failed: {}",
            io::Error::last_os_error()
        );
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };

        assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(supervisor, &mut status, 0) },
            supervisor
        );
        let mut pollfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut pollfd, 1, 2_000) };
        if polled != 1 {
            let _ = unsafe { libc::kill(launcher, libc::SIGKILL) };
        }
        assert_eq!(polled, 1, "launcher survived its parent");
    }

    #[test]
    fn launcher_guard_kills_and_reaps_on_early_return() {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) as RawFd };
        assert!(
            pidfd >= 0,
            "pidfd_open failed: {}",
            io::Error::last_os_error()
        );
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };

        drop(LauncherChild::new(child));

        let mut pollfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut pollfd, 1, 0) }, 1);
    }
}
