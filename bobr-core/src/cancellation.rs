use std::ffi::CString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const FIFO_NAME: &str = "cancel";

/// A shared, cloneable cancellation flag and broadcast channel.
///
/// Clones share one flag and one named FIFO. Cancelling any handle latches the
/// flag and closes the FIFO's sole writer, so every current or future reader
/// observes EOF.
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

struct CancellationInner {
    cancelled: AtomicBool,
    writer: Mutex<Option<File>>,
    fifo_path: PathBuf,
    _control_dir: tempfile::TempDir,
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .field("fifo_path", &self.inner.fifo_path)
            .finish()
    }
}

impl CancellationToken {
    /// Creates a fresh token and its private cancellation FIFO.
    pub fn new() -> io::Result<Self> {
        let control_dir = tempfile::Builder::new()
            .prefix("bobr-cancellation-")
            .tempdir()?;
        std::fs::set_permissions(control_dir.path(), std::fs::Permissions::from_mode(0o700))?;
        let fifo_path = control_dir.path().join(FIFO_NAME);
        create_fifo(&fifo_path)?;
        std::fs::set_permissions(&fifo_path, std::fs::Permissions::from_mode(0o600))?;

        // A nonblocking FIFO writer can only be opened while at least one
        // reader exists. Keep this temporary reader until the sole writer is
        // installed, then drop it; cancellation is represented by dropping
        // that writer.
        let reader = open_fifo(&fifo_path, false)?;
        let writer = open_fifo(&fifo_path, true)?;
        drop(reader);

        Ok(Self {
            inner: Arc::new(CancellationInner {
                cancelled: AtomicBool::new(false),
                writer: Mutex::new(Some(writer)),
                fifo_path,
                _control_dir: control_dir,
            }),
        })
    }

    /// Marks the token cancelled and broadcasts cancellation to FIFO readers.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner
            .writer
            .lock()
            .expect("cancellation writer mutex poisoned")
            .take();
    }

    /// Returns whether the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Path of the FIFO whose EOF broadcasts cancellation.
    pub fn fifo_path(&self) -> &Path {
        &self.inner.fifo_path
    }
}

fn create_fifo(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "FIFO path contains NUL"))?;
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_fifo(path: &Path, write: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(!write)
        .write(write)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn open_reader(token: &CancellationToken) -> File {
        open_fifo(token.fifo_path(), false).unwrap()
    }

    fn observes_eof(file: &File) -> bool {
        let mut byte = 0_u8;
        let result = unsafe { libc::read(file.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
        result == 0
    }

    #[test]
    fn cancel_is_latched_and_idempotent() {
        let token = CancellationToken::new().unwrap();
        let reader = open_reader(&token);

        token.cancel();
        token.cancel();

        assert!(token.is_cancelled());
        assert!(observes_eof(&reader));
    }

    #[test]
    fn one_cancel_wakes_all_readers_and_late_readers() {
        let token = CancellationToken::new().unwrap();
        let readers: Vec<_> = (0..4).map(|_| open_reader(&token)).collect();

        token.cancel();

        assert!(readers.iter().all(observes_eof));
        assert!(observes_eof(&open_reader(&token)));
    }

    #[test]
    fn concurrent_cancel_is_safe() {
        let token = CancellationToken::new().unwrap();
        let reader = open_reader(&token);
        let barrier = Arc::new(Barrier::new(9));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let token = token.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    token.cancel();
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        assert!(token.is_cancelled());
        assert!(observes_eof(&reader));
    }

    #[test]
    fn reader_open_racing_cancel_observes_eof() {
        for _ in 0..64 {
            let token = CancellationToken::new().unwrap();
            let opener = token.clone();
            let barrier = Arc::new(Barrier::new(2));
            let child_barrier = barrier.clone();
            let thread = thread::spawn(move || {
                child_barrier.wait();
                open_reader(&opener)
            });
            barrier.wait();
            token.cancel();
            assert!(observes_eof(&thread.join().unwrap()));
        }
    }

    #[test]
    fn descriptors_are_close_on_exec() {
        let token = CancellationToken::new().unwrap();
        let reader = open_reader(&token);
        let reader_flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFD) };
        let writer = token.inner.writer.lock().unwrap();
        let writer_flags =
            unsafe { libc::fcntl(writer.as_ref().unwrap().as_raw_fd(), libc::F_GETFD) };

        assert_ne!(reader_flags & libc::FD_CLOEXEC, 0);
        assert_ne!(writer_flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn exec_child_does_not_keep_writer_alive() {
        let token = CancellationToken::new().unwrap();
        let reader = open_reader(&token);
        let mut ready_fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(ready_fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let mut ready_read = unsafe { File::from_raw_fd(ready_fds[0]) };
        let ready_write = unsafe { OwnedFd::from_raw_fd(ready_fds[1]) };
        let flags = unsafe { libc::fcntl(ready_write.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe {
                libc::fcntl(
                    ready_write.as_raw_fd(),
                    libc::F_SETFD,
                    flags & !libc::FD_CLOEXEC,
                )
            },
            0
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf x >&{}; exec sleep 60",
                ready_write.as_raw_fd()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        drop(ready_write);
        let mut ready = [0_u8; 1];
        ready_read.read_exact(&mut ready).unwrap();

        token.cancel();

        let saw_eof = observes_eof(&reader);
        let _ = child.kill();
        child.wait().unwrap();
        assert!(saw_eof);
    }

    #[test]
    fn fifo_has_private_permissions_and_lifetime() {
        let token = CancellationToken::new().unwrap();
        let fifo = token.fifo_path().to_path_buf();
        let dir = fifo.parent().unwrap().to_path_buf();
        let fifo_mode = std::fs::metadata(&fifo).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(fifo_mode, 0o600);
        assert_eq!(dir_mode, 0o700);

        let clone = token.clone();
        drop(token);
        assert!(fifo.exists());
        drop(clone);
        assert!(!dir.exists());
    }
}
