use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::CString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const FIFO_NAME: &str = "cancel";

/// A shared, cloneable cancellation flag and broadcast channel.
///
/// Clones share one flag and one named FIFO. Cancelling any handle writes one
/// byte to the FIFO and closes its writer. Readers only poll the FIFO and
/// never consume the byte, so every current or future reader observes the
/// latched signal.
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

struct CancellationInner {
    cancelled: AtomicBool,
    _reader: File,
    writer: Mutex<Option<File>>,
    fifo: CancellationFifo,
}

// Created only after mkfifo succeeds, so failure cannot unlink an existing path.
struct CancellationFifo {
    path: PathBuf,
    _temporary_dir: Option<tempfile::TempDir>,
}

impl Drop for CancellationFifo {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .field("fifo_path", &self.inner.fifo.path)
            .finish()
    }
}

impl CancellationToken {
    /// Creates a fresh token and FIFO in an owned private temporary directory.
    pub fn new() -> io::Result<Self> {
        let temporary_dir = tempfile::Builder::new()
            .prefix("bobr-cancellation-")
            .tempdir()?;
        std::fs::set_permissions(temporary_dir.path(), std::fs::Permissions::from_mode(0o700))?;
        Self::new_with_fifo(temporary_dir.path().join(FIFO_NAME), Some(temporary_dir))
    }

    /// Creates a token's FIFO directly in the work directory of one run.
    ///
    /// The caller owns `run_work_dir` and must have created it exclusively for
    /// this run, with no write access for other users. The last token removes
    /// only its FIFO, leaving the work directory and its other contents intact.
    pub fn new_in(run_work_dir: &Path) -> io::Result<Self> {
        Self::new_with_fifo(run_work_dir.join(FIFO_NAME), None)
    }

    fn new_with_fifo(
        fifo_path: PathBuf,
        temporary_dir: Option<tempfile::TempDir>,
    ) -> io::Result<Self> {
        create_fifo(&fifo_path)?;
        let fifo = CancellationFifo {
            path: fifo_path,
            _temporary_dir: temporary_dir,
        };
        std::fs::set_permissions(&fifo.path, std::fs::Permissions::from_mode(0o600))?;

        // A nonblocking FIFO writer can only be opened while at least one
        // reader exists. Keep this reader for the token's lifetime so an early
        // cancellation can always write the latched signal for late readers.
        let reader = open_fifo(&fifo.path, false)?;
        let writer = open_fifo(&fifo.path, true)?;

        Ok(Self {
            inner: Arc::new(CancellationInner {
                cancelled: AtomicBool::new(false),
                _reader: reader,
                writer: Mutex::new(Some(writer)),
                fifo,
            }),
        })
    }

    /// Marks the token cancelled and broadcasts cancellation to FIFO readers.
    pub fn cancel(&self) {
        let mut writer_slot = self
            .inner
            .writer
            .lock()
            .expect("cancellation writer mutex poisoned");
        let Some(writer) = writer_slot.as_mut() else {
            return;
        };
        writer
            .write_all(&[1])
            .expect("failed to latch cancellation in private FIFO");
        self.inner.cancelled.store(true, Ordering::SeqCst);
        writer_slot.take();
    }

    /// Returns whether the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Creates a receiver that can be passed directly in a runtime input.
    ///
    /// Keep a token alive until every runtime call using the channel completes.
    pub fn receiver(&self) -> io::Result<CancellationReceiver> {
        CancellationReceiver::open(self.inner.fifo.path.clone())
    }
}

/// An observation-only, serializable receiver for a cancellation broadcast.
///
/// Serialization carries only the FIFO path. Deserialization opens and checks
/// a fresh descriptor in the receiving process. Receivers never consume the
/// signal byte, so one cancellation remains visible to every receiver.
#[derive(Debug)]
pub struct CancellationReceiver {
    fifo_path: PathBuf,
    fifo: File,
}

impl CancellationReceiver {
    fn open(fifo_path: PathBuf) -> io::Result<Self> {
        let fifo = open_fifo(&fifo_path, false).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to open cancellation FIFO '{}': {error}",
                    fifo_path.display()
                ),
            )
        })?;
        let metadata = fifo.metadata()?;
        if !metadata.file_type().is_fifo() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cancellation path is not a FIFO: '{}'", fifo_path.display()),
            ));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "cancellation FIFO '{}' is not owned by the runtime user",
                    fifo_path.display()
                ),
            ));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "cancellation FIFO '{}' is accessible by other users",
                    fifo_path.display()
                ),
            ));
        }
        Ok(Self { fifo_path, fifo })
    }

    /// Checks for cancellation without blocking or consuming the signal.
    ///
    /// Loss of the producer (hangup) is also treated as cancellation.
    pub fn is_cancelled(&self) -> io::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: self.fifo.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
            if result > 0 {
                if pollfd.revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                }
                return Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0);
            }
            if result == 0 {
                return Ok(false);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Serialize for CancellationReceiver {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.fifo_path.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CancellationReceiver {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let path = PathBuf::deserialize(deserializer)?;
        Self::open(path).map_err(serde::de::Error::custom)
    }
}

impl AsFd for CancellationReceiver {
    /// Borrows the descriptor for polling only; never read from it.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fifo.as_fd()
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
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn open_reader(token: &CancellationToken) -> CancellationReceiver {
        token.receiver().unwrap()
    }

    fn observes_signal(receiver: &CancellationReceiver) -> bool {
        receiver.is_cancelled().unwrap()
    }

    #[test]
    fn cancel_is_latched_and_idempotent() {
        let token = CancellationToken::new().unwrap();
        let reader = open_reader(&token);

        assert!(!token.is_cancelled());
        assert!(!observes_signal(&reader));
        token.cancel();
        token.cancel();

        assert!(token.is_cancelled());
        assert!(observes_signal(&reader));
        assert!(observes_signal(&reader));
    }

    #[test]
    fn one_cancel_wakes_all_readers_and_late_readers() {
        let token = CancellationToken::new().unwrap();
        let readers: Vec<_> = (0..4).map(|_| open_reader(&token)).collect();

        token.cancel();

        assert!(readers.iter().all(observes_signal));
        assert!(observes_signal(&open_reader(&token)));
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
        assert!(observes_signal(&reader));
    }

    #[test]
    fn reader_open_racing_cancel_observes_signal() {
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
            assert!(observes_signal(&thread.join().unwrap()));
        }
    }

    #[test]
    fn descriptors_are_close_on_exec() {
        let token = CancellationToken::new().unwrap();
        let receiver = token.receiver().unwrap();
        let receiver_flags = unsafe { libc::fcntl(receiver.as_fd().as_raw_fd(), libc::F_GETFD) };
        let reader_flags = unsafe { libc::fcntl(token.inner._reader.as_raw_fd(), libc::F_GETFD) };
        let writer = token.inner.writer.lock().unwrap();
        let writer_flags =
            unsafe { libc::fcntl(writer.as_ref().unwrap().as_raw_fd(), libc::F_GETFD) };

        assert_ne!(reader_flags & libc::FD_CLOEXEC, 0);
        assert_ne!(writer_flags & libc::FD_CLOEXEC, 0);
        assert_ne!(receiver_flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn an_extra_writer_does_not_hide_cancellation() {
        let token = CancellationToken::new().unwrap();
        let reader = open_reader(&token);
        let inherited_writer = open_fifo(&token.inner.fifo.path, true).unwrap();

        token.cancel();

        assert!(observes_signal(&reader));
        drop(inherited_writer);
    }

    #[test]
    fn fifo_has_private_permissions_and_lifetime() {
        let token = CancellationToken::new().unwrap();
        let fifo = token.inner.fifo.path.clone();
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

    #[test]
    fn run_token_removes_only_its_fifo_with_the_last_clone() {
        let run_work = tempfile::tempdir().unwrap();
        std::fs::set_permissions(run_work.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let sentinel = run_work.path().join("sentinel");
        std::fs::write(&sentinel, b"keep").unwrap();
        let token = CancellationToken::new_in(run_work.path()).unwrap();
        let fifo_path = run_work.path().join(FIFO_NAME);

        assert_eq!(token.inner.fifo.path, fifo_path);
        assert!(!run_work.path().join(".control").exists());
        assert_eq!(
            std::fs::metadata(run_work.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(
            std::fs::metadata(&fifo_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let clone = token.clone();
        drop(token);
        assert!(fifo_path.exists());
        drop(clone);
        assert!(!fifo_path.exists());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(run_work.path()).unwrap().count(), 1);
        assert_eq!(
            std::fs::metadata(run_work.path()).unwrap().mode() & 0o777,
            0o750
        );
    }

    #[test]
    fn creation_never_overwrites_or_removes_an_existing_cancel_path() {
        let run_work = tempfile::tempdir().unwrap();
        let path = run_work.path().join(FIFO_NAME);
        std::fs::write(&path, b"existing").unwrap();
        let error = CancellationToken::new_in(run_work.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        std::fs::remove_file(&path).unwrap();

        let token = CancellationToken::new_in(run_work.path()).unwrap();
        assert!(CancellationToken::new_in(run_work.path()).is_err());
        assert!(!token.receiver().unwrap().is_cancelled().unwrap());
    }

    #[test]
    fn receiver_outlives_token_without_keeping_the_channel_path_alive() {
        let token = CancellationToken::new().unwrap();
        let receiver = token.receiver().unwrap();
        let encoded = serde_json::to_vec(&receiver).unwrap();
        drop(token);

        assert!(receiver.is_cancelled().unwrap());
        assert!(serde_json::from_slice::<CancellationReceiver>(&encoded).is_err());
    }

    #[test]
    fn serialization_carries_only_the_path_and_opens_a_new_receiver() {
        let token = CancellationToken::new().unwrap();
        let receiver = token.receiver().unwrap();
        let encoded = serde_json::to_value(&receiver).unwrap();
        assert_eq!(
            encoded,
            serde_json::to_value(&token.inner.fifo.path).unwrap()
        );
        let decoded: CancellationReceiver = serde_json::from_value(encoded).unwrap();
        assert_ne!(receiver.as_fd().as_raw_fd(), decoded.as_fd().as_raw_fd());
        assert!(!decoded.is_cancelled().unwrap());

        token.cancel();
        assert!(receiver.is_cancelled().unwrap());
        assert!(decoded.is_cancelled().unwrap());
    }

    #[test]
    fn cancellation_survives_serialization_before_and_after_cancel() {
        for cancel_before_serialization in [false, true] {
            let token = CancellationToken::new().unwrap();
            let receiver = token.receiver().unwrap();
            if cancel_before_serialization {
                token.cancel();
            }
            let encoded = serde_json::to_vec(&receiver).unwrap();
            drop(receiver);
            token.cancel();

            let decoded: CancellationReceiver = serde_json::from_slice(&encoded).unwrap();
            assert!(decoded.is_cancelled().unwrap());
        }
    }

    #[test]
    fn deserialization_rejects_missing_paths_regular_files_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let regular = temp.path().join("regular");
        let link = temp.path().join("link");
        std::fs::write(&regular, []).unwrap();
        let token = CancellationToken::new_in(temp.path()).unwrap();
        std::os::unix::fs::symlink(&token.inner.fifo.path, &link).unwrap();
        for path in [regular, link, temp.path().join("missing")] {
            let encoded = serde_json::to_value(path).unwrap();
            assert!(serde_json::from_value::<CancellationReceiver>(encoded).is_err());
        }
    }

    #[test]
    fn deserialization_rechecks_fifo_permissions() {
        let token = CancellationToken::new().unwrap();
        let encoded = serde_json::to_value(token.receiver().unwrap()).unwrap();
        std::fs::set_permissions(
            &token.inner.fifo.path,
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();

        let error = serde_json::from_value::<CancellationReceiver>(encoded).unwrap_err();
        assert!(error.to_string().contains("accessible by other users"));
        assert!(token.receiver().is_err());
    }

    #[test]
    fn deserialization_rejects_a_foreign_owner_when_privileged() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let token = CancellationToken::new().unwrap();
        let encoded = serde_json::to_value(token.receiver().unwrap()).unwrap();
        std::os::unix::fs::lchown(&token.inner.fifo.path, Some(123), None).unwrap();

        let error = serde_json::from_value::<CancellationReceiver>(encoded).unwrap_err();
        assert!(error.to_string().contains("not owned by the runtime user"));
    }
}
