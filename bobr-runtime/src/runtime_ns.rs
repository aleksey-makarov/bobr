//! Namespace runtime implementation.
//!
//! [`crate::runtime_ns::NsRuntime`] executes each typed runtime function call in
//! a fresh child process that enters a Linux user namespace before running the
//! worker entrypoint. Calls are marshalled over length-prefixed frames using a
//! [`crate::runtime_ns::WireCodec`].
//!
//! The parent side constructs [`crate::runtime_ns::NsRuntime`]. The child side
//! must detect [`crate::runtime_ns::worker_invocation_from_env`] and call
//! [`crate::runtime_ns::run_worker`] with a registry of
//! [`crate::runtime_ns::NsFunction`] values when the current executable is
//! launched in worker mode.

use crate::runtime::{Runtime, RuntimeError, RuntimeFunction, RuntimeResult};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
const MAX_IDMAP_HELPER_STDERR_LEN: usize = 64 * 1024;
const WORKER_ARG: &str = "--bobr-runtime-worker";
const WORKER_PROTOCOL_READ_ARG: &str = "--protocol-read-fd";
const WORKER_PROTOCOL_WRITE_ARG: &str = "--protocol-write-fd";
const SELF_EXE_PATH: &str = "/proc/self/exe";
const SUBUID_PATH: &str = "/etc/subuid";
const SUBGID_PATH: &str = "/etc/subgid";

/// Codec used by the namespace runtime wire protocol.
///
/// A codec encodes both control messages and runtime function payloads. The
/// framing layer treats encoded values as opaque byte payloads.
pub trait WireCodec {
    /// Encode a serializable value into a byte payload.
    fn encode<T: Serialize>(value: &T) -> RuntimeResult<Vec<u8>>;

    /// Decode a value from a byte payload.
    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> RuntimeResult<T>;
}

/// JSON implementation of [`WireCodec`].
///
/// This is the default codec used by [`NsRuntime`] and [`NsFunction`].
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonCodec;

impl WireCodec for JsonCodec {
    fn encode<T: Serialize>(value: &T) -> RuntimeResult<Vec<u8>> {
        serde_json::to_vec(value).map_err(|error| RuntimeError::new(error.to_string()))
    }

    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> RuntimeResult<T> {
        serde_json::from_slice(bytes).map_err(|error| RuntimeError::new(error.to_string()))
    }
}

/// Runtime that executes functions in a child Linux user namespace.
///
/// Each call to [`Runtime::run`] starts a fresh worker process, configures a
/// Linux user namespace for that process, sends one framed request, waits for
/// one framed response, waits for the process to exit, and decodes the typed
/// output.
///
/// The worker process is started from an executable file descriptor captured
/// from `/proc/self/exe` when this runtime is constructed, so applications using
/// this runtime must route the worker invocation through
/// [`worker_invocation_from_env`] and [`run_worker`].
///
/// Worker standard input, output, and error are redirected to `/dev/null`.
/// Worker environment is empty; variables from the parent process, including
/// `PATH`, are not inherited.
///
/// Runtime functions must return data through their typed result, not through
/// process stdio. If a runtime function starts subprocesses and needs their
/// output for diagnostics or results, the function implementation must capture
/// that output explicitly, for example by using `Command::output` or piped
/// `Stdio`; inherited subprocess stdout and stderr are discarded.
///
/// Calls do not have a timeout by default. Use
/// [`NsRuntime::with_call_timeout`] to bound each `Runtime::run` call.
pub struct NsRuntime<C = JsonCodec> {
    executable: File,
    idmap: HostIdmap,
    tools: NsTools,
    next_id: AtomicU64,
    call_timeout: Option<Duration>,
    _codec: PhantomData<fn() -> C>,
}

impl NsRuntime<JsonCodec> {
    /// Start a namespace runtime using the default [`JsonCodec`].
    ///
    /// This is a convenience constructor for applications that use JSON for
    /// both control messages and function payloads. It is equivalent to
    /// [`NsRuntime::<JsonCodec>::new_with_codec`].
    ///
    /// Construction opens `/proc/self/exe`, resolves the host uid/gid mapping,
    /// and resolves the `newuidmap`/`newgidmap` helper paths. The worker
    /// process itself is started separately for each [`Runtime::run`] call.
    ///
    /// The application must route the worker invocation through
    /// [`worker_invocation_from_env`] and [`run_worker`] before running normal
    /// application logic.
    pub fn new() -> RuntimeResult<Self> {
        Self::new_with_codec()
    }
}

impl<C> NsRuntime<C>
where
    C: WireCodec,
{
    /// Start a namespace runtime using codec `C`.
    ///
    /// Use this constructor when parent and worker should communicate with a
    /// codec other than [`JsonCodec`]. The same codec type must be used for:
    ///
    /// - the parent-side [`NsRuntime<C>`],
    /// - every worker-side [`NsFunction<C>`],
    /// - the child-side [`run_worker::<C>`] call.
    ///
    /// This constructor resolves all host-side prerequisites. It can fail if
    /// `/proc/self/exe` cannot be opened, `/etc/subuid` or
    /// `/etc/subgid` does not contain a usable range for the current user, or
    /// `newuidmap`/`newgidmap` are missing. Per-call namespace setup failures
    /// are reported by [`Runtime::run`].
    pub fn new_with_codec() -> RuntimeResult<Self> {
        let executable = open_current_executable()?;
        let idmap = HostIdmap::from_host_environment()?;
        let tools = NsTools::resolve()?;

        Ok(Self {
            executable,
            idmap,
            tools,
            next_id: AtomicU64::new(0),
            call_timeout: None,
            _codec: PhantomData,
        })
    }

    /// Return a runtime that times out each namespace call after `timeout`.
    ///
    /// The timeout applies to the whole [`Runtime::run`] call: worker launch,
    /// namespace setup, request/response I/O, and child process exit. On
    /// timeout the parent kills the worker process group and reaps the worker.
    ///
    /// A zero duration is allowed and means that calls time out immediately.
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = Some(timeout);
        self
    }

    /// Return a runtime with per-call timeouts disabled.
    ///
    /// This restores the default behavior of [`NsRuntime::new`] and
    /// [`NsRuntime::new_with_codec`].
    pub fn without_call_timeout(mut self) -> Self {
        self.call_timeout = None;
        self
    }

    /// Return the configured per-call timeout.
    ///
    /// `None` means namespace runtime calls are allowed to run without a
    /// runtime-enforced deadline.
    pub fn call_timeout(&self) -> Option<Duration> {
        self.call_timeout
    }
}

impl<C> Runtime for NsRuntime<C>
where
    C: WireCodec,
{
    fn run<F>(&self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
    where
        F: RuntimeFunction,
    {
        let input = C::encode(&input)
            .map_err(|error| RuntimeError::new(format!("failed to encode input: {error}")))?;
        let output = self.call_erased(function.name(), input)?;
        C::decode(&output)
            .map_err(|error| RuntimeError::new(format!("failed to decode output: {error}")))
    }
}

impl<C> NsRuntime<C>
where
    C: WireCodec,
{
    fn call_erased(&self, function_name: &str, input: Vec<u8>) -> RuntimeResult<Vec<u8>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let deadline = CallDeadline::new(function_name, self.call_timeout);
        deadline.ensure("starting call")?;

        let launch = fork_ns_worker(&self.executable)?;
        if let Err(error) = complete_namespace_setup(
            &launch.child,
            &launch.handshake,
            &self.tools,
            &self.idmap,
            &deadline,
        ) {
            terminate_child(&launch.child);
            drop(launch.handshake);
            drop(launch.protocol_writer);
            drop(launch.protocol_reader);
            return Err(error);
        }

        self.call_worker(function_name, id, input, launch, &deadline)
    }

    fn call_worker(
        &self,
        function_name: &str,
        id: u64,
        input: Vec<u8>,
        launch: NsLaunch,
        deadline: &CallDeadline<'_>,
    ) -> RuntimeResult<Vec<u8>> {
        let NsLaunch {
            child,
            handshake,
            protocol_writer,
            protocol_reader,
        } = launch;
        drop(handshake);

        let call_result = send_call_and_read_response::<C>(
            function_name,
            id,
            input,
            protocol_writer.as_raw_fd(),
            protocol_reader.as_raw_fd(),
            deadline,
        );
        drop(protocol_writer);
        drop(protocol_reader);

        let wait_result = wait_for_child(child, deadline);
        match (call_result, wait_result) {
            (Ok(output), Ok(())) => Ok(output),
            (Ok(_), Err(wait_error)) => Err(wait_error),
            (Err(call_error), Ok(())) => Err(call_error),
            (Err(call_error), Err(wait_error)) => {
                Err(RuntimeError::new(format!("{call_error}; {wait_error}")))
            }
        }
    }
}

fn send_call_and_read_response<C>(
    function_name: &str,
    id: u64,
    input: Vec<u8>,
    protocol_write_fd: RawFd,
    protocol_read_fd: RawFd,
    deadline: &CallDeadline<'_>,
) -> RuntimeResult<Vec<u8>>
where
    C: WireCodec,
{
    let request = ParentToChild::Call {
        id,
        function: function_name.to_string(),
    };
    let request = C::encode(&request)?;
    write_frame_until(protocol_write_fd, &request, deadline, "protocol request")?;
    write_frame_until(protocol_write_fd, &input, deadline, "protocol input")?;

    let response = read_frame_until(protocol_read_fd, deadline, "protocol response")?
        .ok_or_else(|| RuntimeError::new("namespace runtime exited without response"))?;
    match C::decode::<ChildToParent>(&response)? {
        ChildToParent::Ok { id: response_id } if response_id == id => {
            read_frame_until(protocol_read_fd, deadline, "protocol output")?
                .ok_or_else(|| RuntimeError::new("namespace runtime exited without output frame"))
        }
        ChildToParent::Err {
            id: response_id,
            message,
        } if response_id == id => Err(RuntimeError::new(format!(
            "namespace runtime failed while running '{}': {message}",
            function_name
        ))),
        response => Err(RuntimeError::new(format!(
            "namespace runtime returned response for the wrong call: expected id {id}, got id {}",
            response.id()
        ))),
    }
}

/// Worker-side erased wrapper around a typed [`RuntimeFunction`].
///
/// `NsFunction` is used only by the namespace worker registry. Parent-side
/// callers keep using typed function values directly through [`Runtime::run`].
/// The wrapper exists so a single worker registry can store functions with
/// different input and output types.
pub struct NsFunction<C = JsonCodec> {
    name: &'static str,
    call: Box<ErasedNsCall>,
    _codec: PhantomData<fn() -> C>,
}

type ErasedNsCall = dyn Fn(&[u8]) -> RuntimeResult<Vec<u8>> + Send + Sync + 'static;

impl<C> NsFunction<C>
where
    C: WireCodec + 'static,
{
    /// Wrap a typed [`RuntimeFunction`] for use by a namespace worker.
    ///
    /// The parent side can call typed functions directly through
    /// [`NsRuntime::run`], but the worker side needs a single registry that can
    /// store functions with different input and output types. `NsFunction`
    /// performs that type erasure.
    ///
    /// The wrapper stores the function's [`RuntimeFunction::name`] and an
    /// erased closure. When a request arrives, the closure decodes the input
    /// bytes with `C`, calls [`RuntimeFunction::call`], and encodes the typed
    /// output with `C`.
    ///
    /// The wrapped function must be `'static` because it is stored in the
    /// worker registry for the lifetime of the worker loop.
    pub fn new<F>(function: F) -> Self
    where
        F: RuntimeFunction + 'static,
    {
        let name = function.name();
        let call = Box::new(move |input: &[u8]| {
            let input = C::decode::<F::Input>(input).map_err(|error| {
                RuntimeError::new(format!("invalid input for '{name}': {error}"))
            })?;
            let output = function.call(input)?;
            C::encode(&output).map_err(|error| {
                RuntimeError::new(format!("invalid output from '{name}': {error}"))
            })
        });
        Self {
            name,
            call,
            _codec: PhantomData,
        }
    }
}

impl<C> NsFunction<C> {
    /// Return the stable function name registered in the worker.
    pub fn name(&self) -> &'static str {
        self.name
    }

    fn call_erased(&self, input: &[u8]) -> RuntimeResult<Vec<u8>> {
        (self.call)(input)
    }
}

/// Worker-mode protocol descriptor passed to [`run_worker`].
///
/// Applications do not construct this value directly. Call
/// [`worker_invocation_from_env`] at program startup; if it returns `Some`,
/// pass the invocation to [`run_worker`] with the worker registry.
#[derive(Debug, Clone, Copy)]
pub struct WorkerInvocation {
    protocol_read_fd: RawFd,
    protocol_write_fd: RawFd,
}

impl WorkerInvocation {
    fn new(protocol_read_fd: RawFd, protocol_write_fd: RawFd) -> RuntimeResult<Self> {
        if protocol_read_fd <= libc::STDERR_FILENO {
            return Err(RuntimeError::new(format!(
                "namespace runtime protocol read fd {protocol_read_fd} must not be stdio"
            )));
        }
        if protocol_write_fd <= libc::STDERR_FILENO {
            return Err(RuntimeError::new(format!(
                "namespace runtime protocol write fd {protocol_write_fd} must not be stdio"
            )));
        }
        if protocol_read_fd == protocol_write_fd {
            return Err(RuntimeError::new(format!(
                "namespace runtime protocol read and write fd are both {protocol_read_fd}"
            )));
        }
        Ok(Self {
            protocol_read_fd,
            protocol_write_fd,
        })
    }
}

/// Return the namespace worker invocation encoded in this process's arguments.
///
/// Parent processes get `Ok(None)` and should continue normal application
/// startup. A namespace worker process gets `Ok(Some(...))` and should
/// immediately call [`run_worker`]. Malformed worker arguments are reported as
/// [`RuntimeError`] values.
pub fn worker_invocation_from_env() -> RuntimeResult<Option<WorkerInvocation>> {
    worker_invocation_from_args(env::args_os())
}

fn worker_invocation_from_args<I>(args: I) -> RuntimeResult<Option<WorkerInvocation>>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _arg0 = args.next();
    let Some(mode) = args.next() else {
        return Ok(None);
    };
    if mode.as_os_str() != OsStr::new(WORKER_ARG) {
        return Ok(None);
    }

    expect_worker_arg(&mut args, WORKER_PROTOCOL_READ_ARG)?;
    let protocol_read_fd =
        parse_worker_fd_arg(next_worker_arg(&mut args, WORKER_PROTOCOL_READ_ARG)?)?;
    expect_worker_arg(&mut args, WORKER_PROTOCOL_WRITE_ARG)?;
    let protocol_write_fd =
        parse_worker_fd_arg(next_worker_arg(&mut args, WORKER_PROTOCOL_WRITE_ARG)?)?;

    if let Some(extra) = args.next() {
        return Err(RuntimeError::new(format!(
            "unexpected bobr runtime worker argument '{}'",
            extra.to_string_lossy()
        )));
    }

    WorkerInvocation::new(protocol_read_fd, protocol_write_fd).map(Some)
}

fn expect_worker_arg(
    args: &mut impl Iterator<Item = OsString>,
    expected: &'static str,
) -> RuntimeResult<()> {
    let actual = next_worker_arg(args, expected)?;
    if actual.as_os_str() == OsStr::new(expected) {
        Ok(())
    } else {
        Err(RuntimeError::new(format!(
            "expected bobr runtime worker argument {expected}, got '{}'",
            actual.to_string_lossy()
        )))
    }
}

fn next_worker_arg(
    args: &mut impl Iterator<Item = OsString>,
    expected: &'static str,
) -> RuntimeResult<OsString> {
    args.next().ok_or_else(|| {
        RuntimeError::new(format!("missing bobr runtime worker argument {expected}"))
    })
}

fn parse_worker_fd_arg(value: OsString) -> RuntimeResult<RawFd> {
    let value = value.into_string().map_err(|value| {
        RuntimeError::new(format!(
            "bobr runtime worker fd '{}' is not valid UTF-8",
            value.to_string_lossy()
        ))
    })?;
    value.parse::<RawFd>().map_err(|error| {
        RuntimeError::new(format!(
            "bobr runtime worker fd '{value}' is not an integer: {error}"
        ))
    })
}

/// Run one namespace worker call.
///
/// The worker reads framed requests from the private protocol fd encoded in
/// `invocation`, dispatches calls by function name through `functions`, and
/// writes framed responses to the private protocol response fd. Standard
/// output and standard error are not protocol streams. When the worker is
/// launched by [`NsRuntime`], all three standard descriptors are redirected to
/// `/dev/null`. Function implementations that launch commands must configure
/// and capture those commands' stdout and stderr themselves if the output is
/// needed; inherited command output is discarded.
///
/// This function handles one call and exits. It catches ordinary unwinding
/// panics from runtime functions and returns them as framed errors, but it
/// cannot catch aborts, process exits, or signals.
///
/// `functions` must contain at most one entry for each function name.
pub fn run_worker<C>(
    invocation: WorkerInvocation,
    functions: Vec<NsFunction<C>>,
) -> RuntimeResult<()>
where
    C: WireCodec,
{
    let mut protocol = open_worker_protocol(invocation)?;
    run_worker_once::<C, _, _>(&mut protocol.reader, &mut protocol.writer, functions)
}

struct WorkerProtocol {
    reader: BufReader<File>,
    writer: BufWriter<File>,
}

fn open_worker_protocol(invocation: WorkerInvocation) -> RuntimeResult<WorkerProtocol> {
    set_fd_cloexec(invocation.protocol_read_fd, true)?;
    set_fd_cloexec(invocation.protocol_write_fd, true)?;

    let reader = unsafe { File::from_raw_fd(invocation.protocol_read_fd) };
    let writer = unsafe { File::from_raw_fd(invocation.protocol_write_fd) };

    Ok(WorkerProtocol {
        reader: BufReader::new(reader),
        writer: BufWriter::new(writer),
    })
}

fn run_worker_once<C, R, W>(
    reader: &mut R,
    writer: &mut W,
    functions: Vec<NsFunction<C>>,
) -> RuntimeResult<()>
where
    C: WireCodec,
    R: Read,
    W: Write,
{
    let mut registry = BTreeMap::<String, NsFunction<C>>::new();
    for function in functions {
        let name = function.name().to_string();
        if registry.insert(name.clone(), function).is_some() {
            return Err(RuntimeError::new(format!(
                "duplicate namespace runtime function '{name}'"
            )));
        }
    }

    let request = read_frame(reader)?
        .ok_or_else(|| RuntimeError::new("bobr runtime worker received no request"))?;
    let ParentToChild::Call { id, function } = C::decode::<ParentToChild>(&request)?;
    let input = read_frame(reader)?
        .ok_or_else(|| RuntimeError::new(format!("missing input frame for call id {id}")))?;

    match registry.get(&function) {
        Some(function_impl) => {
            match panic::catch_unwind(AssertUnwindSafe(|| function_impl.call_erased(&input))) {
                Ok(Ok(output)) => {
                    let response = C::encode(&ChildToParent::Ok { id })?;
                    write_frame(writer, &response)?;
                    write_frame(writer, &output)?;
                }
                Ok(Err(error)) => {
                    let response = C::encode(&ChildToParent::Err {
                        id,
                        message: error.to_string(),
                    })?;
                    write_frame(writer, &response)?;
                }
                Err(payload) => {
                    let response = C::encode(&ChildToParent::Err {
                        id,
                        message: panic_error_message(&function, payload.as_ref()),
                    })?;
                    write_frame(writer, &response)?;
                }
            }
        }
        None => {
            let response = C::encode(&ChildToParent::Err {
                id,
                message: format!("unknown function '{function}'"),
            })?;
            write_frame(writer, &response)?;
        }
    }
    writer.flush()?;

    Ok(())
}

fn panic_error_message(function: &str, payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        format!("function '{function}' panicked: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!("function '{function}' panicked: {message}")
    } else {
        format!("function '{function}' panicked with non-string payload")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ParentToChild {
    Call { id: u64, function: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChildToParent {
    Ok { id: u64 },
    Err { id: u64, message: String },
}

impl ChildToParent {
    fn id(&self) -> u64 {
        match self {
            Self::Ok { id } | Self::Err { id, .. } => *id,
        }
    }
}

struct CallDeadline<'a> {
    function_name: &'a str,
    started_at: Instant,
    timeout: Option<Duration>,
}

impl<'a> CallDeadline<'a> {
    fn new(function_name: &'a str, timeout: Option<Duration>) -> Self {
        Self {
            function_name,
            started_at: Instant::now(),
            timeout,
        }
    }

    fn has_timeout(&self) -> bool {
        self.timeout.is_some()
    }

    fn is_expired(&self) -> bool {
        self.timeout
            .is_some_and(|timeout| self.started_at.elapsed() >= timeout)
    }

    fn ensure(&self, phase: &'static str) -> RuntimeResult<()> {
        self.remaining(phase).map(|_| ())
    }

    fn remaining(&self, phase: &'static str) -> RuntimeResult<Option<Duration>> {
        let Some(timeout) = self.timeout else {
            return Ok(None);
        };
        let elapsed = self.started_at.elapsed();
        if elapsed >= timeout {
            return Err(self.timeout_error(phase));
        }
        Ok(Some(timeout - elapsed))
    }

    fn timeout_error(&self, phase: &'static str) -> RuntimeError {
        let timeout = self
            .timeout
            .map(|timeout| format!("{timeout:?}"))
            .unwrap_or_else(|| "disabled timeout".to_string());
        RuntimeError::new(format!(
            "namespace runtime timed out after {timeout} while running '{}' during {phase}",
            self.function_name
        ))
    }
}

struct NsLaunch {
    child: NsChild,
    handshake: NsHandshake,
    protocol_writer: OwnedFd,
    protocol_reader: OwnedFd,
}

#[derive(Debug, Clone, Copy)]
struct ChildExecFds {
    stdin_read: RawFd,
    stdout_write: RawFd,
    stderr_write: RawFd,
    protocol_read: RawFd,
    protocol_write: RawFd,
    userns_ready_write: RawFd,
    idmap_ready_read: RawFd,
}

#[derive(Debug)]
struct NsChild {
    pid: libc::pid_t,
}

impl NsChild {
    fn pid_u32(&self) -> u32 {
        self.pid as u32
    }
}

struct NsHandshake {
    userns_ready_read: OwnedFd,
    idmap_ready_write: OwnedFd,
}

#[derive(Debug)]
struct NsTools {
    newuidmap: PathBuf,
    newgidmap: PathBuf,
}

impl NsTools {
    fn resolve() -> RuntimeResult<Self> {
        Ok(Self {
            newuidmap: resolve_path_program("newuidmap")?,
            newgidmap: resolve_path_program("newgidmap")?,
        })
    }
}

fn open_current_executable() -> RuntimeResult<File> {
    let executable = File::open(SELF_EXE_PATH)
        .map_err(|error| RuntimeError::new(format!("failed to open {SELF_EXE_PATH}: {error}")))?;
    let executable = move_file_fd_out_of_stdio(executable).map_err(|error| {
        RuntimeError::new(format!(
            "failed to move {SELF_EXE_PATH} fd out of stdio range: {error}"
        ))
    })?;
    set_fd_cloexec(executable.as_raw_fd(), true)?;
    Ok(executable)
}

fn move_file_fd_out_of_stdio(file: File) -> io::Result<File> {
    if file.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(file);
    }

    let duplicated = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        Err(io::Error::last_os_error())
    } else {
        drop(file);
        Ok(unsafe { File::from_raw_fd(duplicated) })
    }
}

#[derive(Debug)]
struct HostIdmap {
    current_uid: u32,
    current_gid: u32,
    subuid: SubidRange,
    subgid: SubidRange,
}

impl HostIdmap {
    fn from_host_environment() -> RuntimeResult<Self> {
        let current_uid = unsafe { libc::geteuid() };
        let current_gid = unsafe { libc::getegid() };
        let owner = SubidOwner::new(current_uid, current_username(current_uid)?);
        let subuid = read_first_subid_range(Path::new(SUBUID_PATH), &owner, SubidKind::Uid)?;
        let subgid = read_first_subid_range(Path::new(SUBGID_PATH), &owner, SubidKind::Gid)?;

        Ok(Self {
            current_uid,
            current_gid,
            subuid,
            subgid,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubidRange {
    base: u32,
    count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubidOwner {
    uid: u32,
    uid_text: String,
    username: Option<String>,
}

impl SubidOwner {
    fn new(uid: u32, username: Option<String>) -> Self {
        Self {
            uid,
            uid_text: uid.to_string(),
            username,
        }
    }

    fn matches(&self, owner: &str) -> bool {
        self.username.as_deref() == Some(owner) || owner == self.uid_text
    }

    fn display_keys(&self) -> String {
        match &self.username {
            Some(username) => format!("{username} or uid {}", self.uid),
            None => format!("uid {}", self.uid),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SubidKind {
    Uid,
    Gid,
}

impl SubidKind {
    fn subid_name(self) -> &'static str {
        match self {
            Self::Uid => "subuid",
            Self::Gid => "subgid",
        }
    }
}

fn fork_ns_worker(executable: &File) -> RuntimeResult<NsLaunch> {
    let Pipe {
        read: protocol_request_read,
        write: protocol_request_write,
    } = Pipe::new()?;
    let protocol_request_read = move_fd_out_of_stdio(protocol_request_read)?;
    let protocol_request_write = move_fd_out_of_stdio(protocol_request_write)?;

    let Pipe {
        read: protocol_response_read,
        write: protocol_response_write,
    } = Pipe::new()?;
    let protocol_response_read = move_fd_out_of_stdio(protocol_response_read)?;
    let protocol_response_write = move_fd_out_of_stdio(protocol_response_write)?;

    let worker_stdin = File::open("/dev/null").map_err(|error| {
        RuntimeError::new(format!(
            "failed to open /dev/null for worker stdin: {error}"
        ))
    })?;
    let worker_stdout = File::options()
        .write(true)
        .open("/dev/null")
        .map_err(|error| {
            RuntimeError::new(format!(
                "failed to open /dev/null for worker stdout: {error}"
            ))
        })?;
    let worker_stderr = File::options()
        .write(true)
        .open("/dev/null")
        .map_err(|error| {
            RuntimeError::new(format!(
                "failed to open /dev/null for worker stderr: {error}"
            ))
        })?;
    let userns_ready = Pipe::new()?;
    let idmap_ready = Pipe::new()?;
    let executable_fd = executable.as_raw_fd();
    let arg0 = CString::new("bobr-runtime-worker").unwrap();
    let worker_arg = CString::new(WORKER_ARG).unwrap();
    let protocol_read_arg = CString::new(WORKER_PROTOCOL_READ_ARG).unwrap();
    let protocol_read_fd_arg = CString::new(protocol_request_read.as_raw_fd().to_string()).unwrap();
    let protocol_write_arg = CString::new(WORKER_PROTOCOL_WRITE_ARG).unwrap();
    let protocol_write_fd_arg =
        CString::new(protocol_response_write.as_raw_fd().to_string()).unwrap();
    let args = [
        arg0,
        worker_arg,
        protocol_read_arg,
        protocol_read_fd_arg,
        protocol_write_arg,
        protocol_write_fd_arg,
    ];
    let mut arg_ptrs = args.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    arg_ptrs.push(std::ptr::null());
    let env_ptrs = empty_worker_envp();

    // Everything above this point runs in the parent before fork, so normal
    // Rust code is fine: allocating Vec/CString values, resolving paths, and
    // constructing rich RuntimeError messages are all safe here.
    //
    // If fork succeeds, the child must immediately enter child_exec_ns_worker().
    // That function runs in the post-fork/pre-exec window, where only the
    // calling thread exists. Locks held by other parent threads at fork time may
    // remain permanently locked in the child, so child-side setup must avoid
    // allocation, formatting, stdio, logging, mutexes, and other Rust runtime
    // conveniences.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(RuntimeError::new(format!(
            "failed to fork namespace runtime: {}",
            io::Error::last_os_error()
        )));
    }
    if pid == 0 {
        child_exec_ns_worker(
            executable_fd,
            &arg_ptrs,
            &env_ptrs,
            ChildExecFds {
                stdin_read: worker_stdin.as_raw_fd(),
                stdout_write: worker_stdout.as_raw_fd(),
                stderr_write: worker_stderr.as_raw_fd(),
                protocol_read: protocol_request_read.as_raw_fd(),
                protocol_write: protocol_response_write.as_raw_fd(),
                userns_ready_write: userns_ready.write_raw(),
                idmap_ready_read: idmap_ready.read_raw(),
            },
        );
    }

    let Pipe {
        read: userns_ready_read,
        write: userns_ready_write,
    } = userns_ready;
    let Pipe {
        read: idmap_ready_read,
        write: idmap_ready_write,
    } = idmap_ready;

    drop(protocol_request_read);
    drop(protocol_response_write);
    drop(worker_stdin);
    drop(worker_stdout);
    drop(worker_stderr);
    drop(userns_ready_write);
    drop(idmap_ready_read);
    set_fd_nonblocking(protocol_request_write.as_raw_fd(), true)?;
    set_fd_nonblocking(protocol_response_read.as_raw_fd(), true)?;
    set_fd_nonblocking(userns_ready_read.as_raw_fd(), true)?;
    set_fd_nonblocking(idmap_ready_write.as_raw_fd(), true)?;

    Ok(NsLaunch {
        child: NsChild { pid },
        handshake: NsHandshake {
            userns_ready_read,
            idmap_ready_write,
        },
        protocol_writer: protocol_request_write,
        protocol_reader: protocol_response_read,
    })
}

fn empty_worker_envp() -> [*const libc::c_char; 1] {
    [std::ptr::null()]
}

fn move_fd_out_of_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }

    let duplicated = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }
}

fn complete_namespace_setup(
    child: &NsChild,
    handshake: &NsHandshake,
    tools: &NsTools,
    idmap: &HostIdmap,
    deadline: &CallDeadline<'_>,
) -> RuntimeResult<()> {
    wait_for_child_userns(handshake.userns_ready_read.as_raw_fd(), deadline)?;
    configure_id_maps(tools, child.pid_u32(), idmap, deadline)?;
    signal_child_ready(handshake.idmap_ready_write.as_raw_fd(), deadline)
}

// Child-only post-fork/pre-exec setup.
//
// Keep this function and the child_* helpers below restricted to simple libc
// calls and fixed byte strings. Do not use format!, String, Vec, Box, PathBuf,
// std::fs, std::env, Command, println/eprintln, tracing/logging, locks,
// channels, panics, or std::process::exit here. Those operations can allocate
// or acquire locks that may have been held by vanished parent threads when
// fork() happened, which can deadlock the child before it reaches exec().
//
// On failure, write a fixed diagnostic to stderr and terminate with _exit().
// _exit() is important: std::process::exit would run Rust/atexit cleanup in
// this fragile process state.
fn child_exec_ns_worker(
    executable_fd: RawFd,
    args: &[*const libc::c_char],
    env: &[*const libc::c_char],
    fds: ChildExecFds,
) -> ! {
    if !child_setup_stdio(fds.stdin_read, fds.stdout_write, fds.stderr_write) {
        unsafe { libc::_exit(127) };
    }
    if unsafe { libc::setpgid(0, 0) } != 0 {
        child_write_stderr(b"failed to create namespace runtime process group\n");
        unsafe { libc::_exit(127) };
    }
    if !child_clear_cloexec(fds.protocol_read) || !child_clear_cloexec(fds.protocol_write) {
        child_write_stderr(b"failed to preserve namespace runtime protocol fds\n");
        unsafe { libc::_exit(127) };
    }
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        child_write_stderr(b"failed to unshare user namespace\n");
        unsafe { libc::_exit(127) };
    }
    if write_handshake_byte(fds.userns_ready_write).is_err() {
        child_write_stderr(b"failed to signal user namespace readiness\n");
        unsafe { libc::_exit(127) };
    }
    if !child_read_handshake_byte(fds.idmap_ready_read) {
        child_write_stderr(b"failed to wait for idmap readiness\n");
        unsafe { libc::_exit(127) };
    }
    if unsafe { libc::setresgid(0, 0, 0) } != 0 {
        child_write_stderr(b"failed to become gid 0 inside user namespace\n");
        unsafe { libc::_exit(127) };
    }
    if unsafe { libc::setresuid(0, 0, 0) } != 0 {
        child_write_stderr(b"failed to become uid 0 inside user namespace\n");
        unsafe { libc::_exit(127) };
    }
    unsafe {
        child_close_fd(fds.userns_ready_write);
        child_close_fd(fds.idmap_ready_read);
        libc::fexecve(executable_fd, args.as_ptr(), env.as_ptr());
    }
    child_write_stderr(b"failed to exec bobr runtime worker\n");
    unsafe { libc::_exit(127) };
}

// The helpers below are used by child_exec_ns_worker() before fexecve(). Keep
// their implementation within the same restricted syscall-only discipline.
fn child_setup_stdio(stdin_read: RawFd, stdout_write: RawFd, stderr_write: RawFd) -> bool {
    child_duplicate_stdio_fd(stdin_read, libc::STDIN_FILENO)
        && child_duplicate_stdio_fd(stdout_write, libc::STDOUT_FILENO)
        && child_duplicate_stdio_fd(stderr_write, libc::STDERR_FILENO)
}

fn child_duplicate_stdio_fd(fd: RawFd, target_fd: RawFd) -> bool {
    if fd == target_fd {
        child_clear_cloexec(fd)
    } else if unsafe { libc::dup2(fd, target_fd) } < 0 {
        false
    } else {
        child_close_fd(fd);
        true
    }
}

fn child_clear_cloexec(fd: RawFd) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return false;
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return false;
    }
    true
}

fn set_fd_cloexec(fd: RawFd, close_on_exec: bool) -> RuntimeResult<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(RuntimeError::new(format!(
            "failed to read flags for fd {fd}: {}",
            io::Error::last_os_error()
        )));
    }

    let flags = if close_on_exec {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(RuntimeError::new(format!(
            "failed to set close-on-exec flag for fd {fd}: {}",
            io::Error::last_os_error()
        )));
    }

    Ok(())
}

fn set_fd_nonblocking(fd: RawFd, nonblocking: bool) -> RuntimeResult<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(RuntimeError::new(format!(
            "failed to read status flags for fd {fd}: {}",
            io::Error::last_os_error()
        )));
    }

    let flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(RuntimeError::new(format!(
            "failed to set nonblocking flag for fd {fd}: {}",
            io::Error::last_os_error()
        )));
    }

    Ok(())
}

fn child_close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn child_write_stderr(message: &'static [u8]) {
    unsafe {
        let _ = libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
    }
}

fn child_read_handshake_byte(fd: RawFd) -> bool {
    let mut byte = [0_u8; 1];
    loop {
        let result = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if result == 1 {
            return true;
        }
        if result == 0 {
            return false;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

fn wait_for_child_userns(fd: RawFd, deadline: &CallDeadline<'_>) -> RuntimeResult<()> {
    read_handshake_byte_until(fd, "child user namespace setup", deadline)
}

fn signal_child_ready(fd: RawFd, deadline: &CallDeadline<'_>) -> RuntimeResult<()> {
    write_handshake_byte_until(fd, "id map readiness", deadline)
}

fn write_handshake_byte(fd: RawFd) -> io::Result<()> {
    let byte = [1_u8; 1];
    let written = unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) };
    if written == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_handshake_byte_until(
    fd: RawFd,
    label: &'static str,
    deadline: &CallDeadline<'_>,
) -> RuntimeResult<()> {
    let mut byte = [0_u8; 1];
    loop {
        let result = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if result == 1 {
            return Ok(());
        }
        if result == 0 {
            return Err(RuntimeError::new(format!(
                "namespace runtime closed {label} pipe before signalling readiness"
            )));
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => poll_fd_until(fd, libc::POLLIN, deadline, label)?,
            _ => {
                return Err(RuntimeError::new(format!(
                    "failed to read namespace runtime {label} pipe: {error}"
                )));
            }
        }
    }
}

fn write_handshake_byte_until(
    fd: RawFd,
    label: &'static str,
    deadline: &CallDeadline<'_>,
) -> RuntimeResult<()> {
    let byte = [1_u8; 1];
    write_all_fd_until(fd, &byte, deadline, label).map_err(|error| {
        RuntimeError::new(format!(
            "failed to signal namespace runtime readiness: {error}"
        ))
    })
}

fn configure_id_maps(
    tools: &NsTools,
    pid: u32,
    idmap: &HostIdmap,
    deadline: &CallDeadline<'_>,
) -> RuntimeResult<()> {
    run_map_command(
        &tools.newuidmap,
        pid,
        [
            ("0", idmap.current_uid, 1),
            ("1", idmap.subuid.base, idmap.subuid.count),
        ],
        deadline,
    )?;
    write_setgroups_deny(pid)?;
    run_map_command(
        &tools.newgidmap,
        pid,
        [
            ("0", idmap.current_gid, 1),
            ("1", idmap.subgid.base, idmap.subgid.count),
        ],
        deadline,
    )
}

fn run_map_command<const N: usize>(
    program: &Path,
    pid: u32,
    ranges: [(&str, u32, u32); N],
    deadline: &CallDeadline<'_>,
) -> RuntimeResult<()> {
    let mut command = Command::new(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command.arg(pid.to_string());
    for (inside, outside, count) in ranges {
        command
            .arg(inside)
            .arg(outside.to_string())
            .arg(count.to_string());
    }
    let mut child = command.spawn().map_err(|error| {
        RuntimeError::new(format!("failed to run '{}': {error}", program.display()))
    })?;
    let mut stderr = child.stderr.take();
    if let Some(stderr) = &stderr {
        set_fd_nonblocking(stderr.as_raw_fd(), true)?;
    }
    let mut stderr_buffer = Vec::new();

    loop {
        if let Some(stderr) = &mut stderr {
            drain_nonblocking(stderr, &mut stderr_buffer)?;
        }
        match child.try_wait().map_err(|error| {
            RuntimeError::new(format!(
                "failed to wait for '{}': {error}",
                program.display()
            ))
        })? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                if let Some(stderr) = &mut stderr {
                    drain_nonblocking(stderr, &mut stderr_buffer)?;
                }
                return Err(RuntimeError::new(format!(
                    "'{}' failed with {}{}",
                    program.display(),
                    status_message(status),
                    command_context(&stderr_buffer)
                )));
            }
            None => {
                if let Err(error) = sleep_until_deadline(deadline, "id map helper") {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
        }
    }
}

fn write_setgroups_deny(pid: u32) -> RuntimeResult<()> {
    let path = PathBuf::from(format!("/proc/{pid}/setgroups"));
    match fs::write(&path, b"deny\n") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::new(format!(
            "failed to write '{}': {error}",
            path.display()
        ))),
    }
}

fn terminate_child(child: &NsChild) {
    kill_child_process_group(child);
    let _ = wait_for_pid(child.pid);
}

fn wait_for_child(child: NsChild, deadline: &CallDeadline<'_>) -> RuntimeResult<()> {
    let status = match wait_for_pid_until(child.pid, deadline, "child exit") {
        Ok(status) => status,
        Err(error) if deadline.is_expired() => {
            kill_child_process_group(&child);
            let _ = wait_for_pid(child.pid);
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    if raw_wait_status_success(status) {
        Ok(())
    } else {
        Err(RuntimeError::new(format!(
            "namespace runtime exited with {}",
            raw_wait_status_message(status)
        )))
    }
}

fn kill_child_process_group(child: &NsChild) {
    if unsafe { libc::kill(-child.pid, libc::SIGKILL) } != 0 {
        unsafe {
            libc::kill(child.pid, libc::SIGKILL);
        }
    }
}

fn wait_for_pid(pid: libc::pid_t) -> RuntimeResult<i32> {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(status);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(RuntimeError::new(format!(
                "failed to wait for namespace runtime pid {pid}: {error}"
            )));
        }
    }
}

fn wait_for_pid_until(
    pid: libc::pid_t,
    deadline: &CallDeadline<'_>,
    phase: &'static str,
) -> RuntimeResult<i32> {
    if !deadline.has_timeout() {
        return wait_for_pid(pid);
    }

    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            return Ok(status);
        }
        if result == 0 {
            sleep_until_deadline(deadline, phase)?;
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(RuntimeError::new(format!(
                "failed to wait for namespace runtime pid {pid}: {error}"
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

fn status_message(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "signal termination".to_string(),
    }
}

fn command_context(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

fn drain_nonblocking(reader: &mut impl Read, output: &mut Vec<u8>) -> RuntimeResult<()> {
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => append_idmap_helper_stderr(output, &buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => {
                return Err(RuntimeError::new(format!(
                    "failed to read command stderr: {error}"
                )));
            }
        }
    }
}

fn append_idmap_helper_stderr(output: &mut Vec<u8>, bytes: &[u8]) {
    let remaining = MAX_IDMAP_HELPER_STDERR_LEN.saturating_sub(output.len());
    output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

fn poll_fd_until(
    fd: RawFd,
    events: libc::c_short,
    deadline: &CallDeadline<'_>,
    phase: &'static str,
) -> RuntimeResult<()> {
    loop {
        let timeout_ms = deadline
            .remaining(phase)?
            .map(duration_to_poll_timeout_ms)
            .unwrap_or(-1);
        let mut poll_fd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if result > 0 {
            if poll_fd.revents & libc::POLLNVAL != 0 {
                return Err(RuntimeError::new(format!(
                    "namespace runtime fd {fd} is invalid during {phase}"
                )));
            }
            return Ok(());
        }
        if result == 0 {
            return Err(deadline.timeout_error(phase));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(RuntimeError::new(format!(
                "failed to poll namespace runtime fd {fd} during {phase}: {error}"
            )));
        }
    }
}

fn duration_to_poll_timeout_ms(duration: Duration) -> libc::c_int {
    let millis = duration.as_millis();
    if millis == 0 {
        1
    } else {
        millis.min(libc::c_int::MAX as u128) as libc::c_int
    }
}

fn sleep_until_deadline(deadline: &CallDeadline<'_>, phase: &'static str) -> RuntimeResult<()> {
    let Some(remaining) = deadline.remaining(phase)? else {
        thread::sleep(Duration::from_millis(10));
        return Ok(());
    };
    thread::sleep(remaining.min(Duration::from_millis(10)));
    Ok(())
}

fn current_username(current_uid: u32) -> RuntimeResult<Option<String>> {
    let mut buffer_len = passwd_buffer_size();

    loop {
        let mut password = unsafe { std::mem::zeroed::<libc::passwd>() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_len];
        let status = unsafe {
            libc::getpwuid_r(
                current_uid,
                &mut password,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };

        if status == 0 {
            if result.is_null() {
                return Ok(None);
            }
            let name = unsafe { CStr::from_ptr(password.pw_name) };
            return name
                .to_str()
                .map(|name| Some(name.to_string()))
                .map_err(|error| {
                    RuntimeError::new(format!(
                        "passwd entry name for euid {current_uid} is not valid UTF-8: {error}"
                    ))
                });
        }

        if status == libc::ERANGE && buffer_len < max_passwd_buffer_size() {
            buffer_len = buffer_len.saturating_mul(2).min(max_passwd_buffer_size());
            continue;
        }

        return Err(RuntimeError::new(format!(
            "failed to look up passwd entry for euid {current_uid}: {}",
            io::Error::from_raw_os_error(status)
        )));
    }
}

fn passwd_buffer_size() -> usize {
    let size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    if size > 0 { size as usize } else { 16 * 1024 }
}

fn max_passwd_buffer_size() -> usize {
    1024 * 1024
}

fn read_first_subid_range(
    path: &Path,
    owner: &SubidOwner,
    kind: SubidKind,
) -> RuntimeResult<SubidRange> {
    let content = fs::read_to_string(path).map_err(|error| {
        RuntimeError::new(format!(
            "failed to read {} file '{}': {error}",
            kind.subid_name(),
            path.display()
        ))
    })?;
    parse_first_subid_range(&content, owner, kind, &path.display().to_string())
}

fn parse_first_subid_range(
    content: &str,
    owner: &SubidOwner,
    kind: SubidKind,
    source: &str,
) -> RuntimeResult<SubidRange> {
    let mut first_match = None;

    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line_owner = line.split_once(':').map_or(line, |(owner, _)| owner);
        if !owner.matches(line_owner) {
            continue;
        }

        let mut parts = line.split(':');
        let (Some(_parsed_owner), Some(base), Some(count), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(RuntimeError::new(format!(
                "malformed {} line {line_number} in {source}: expected <username>:<base>:<count>",
                kind.subid_name()
            )));
        };

        let base = parse_u32_field(base, "base", kind, source, line_number)?;
        let count = parse_u32_field(count, "count", kind, source, line_number)?;
        if count == 0 {
            return Err(RuntimeError::new(format!(
                "{} line {line_number} in {source} has zero count",
                kind.subid_name()
            )));
        }
        base.checked_add(count - 1).ok_or_else(|| {
            RuntimeError::new(format!(
                "{} line {line_number} in {source} range {base}:{count} overflows u32 range",
                kind.subid_name()
            ))
        })?;

        if first_match.is_none() {
            first_match = Some(SubidRange { base, count });
        }
    }

    first_match.ok_or_else(|| {
        RuntimeError::new(format!(
            "{} not configured for {} in {source}",
            kind.subid_name(),
            owner.display_keys()
        ))
    })
}

fn parse_u32_field(
    value: &str,
    field: &str,
    kind: SubidKind,
    source: &str,
    line: usize,
) -> RuntimeResult<u32> {
    value.parse::<u32>().map_err(|error| {
        RuntimeError::new(format!(
            "malformed {} line {line} in {source}: invalid {field} '{value}': {error}",
            kind.subid_name()
        ))
    })
}

fn resolve_path_program(program: &str) -> RuntimeResult<PathBuf> {
    let path = env::var_os("PATH")
        .ok_or_else(|| RuntimeError::new(format!("PATH is not set; cannot find {program}")))?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(RuntimeError::new(format!("{program} not found in PATH")))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

fn write_frame(writer: &mut impl Write, payload: &[u8]) -> RuntimeResult<()> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(RuntimeError::new(format!(
            "frame length {} exceeds limit {}",
            payload.len(),
            MAX_FRAME_LEN
        )));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| RuntimeError::new(format!("frame length {} exceeds u32", payload.len())))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(payload)?;
    Ok(())
}

fn write_frame_until(
    fd: RawFd,
    payload: &[u8],
    deadline: &CallDeadline<'_>,
    phase: &'static str,
) -> RuntimeResult<()> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(RuntimeError::new(format!(
            "frame length {} exceeds limit {}",
            payload.len(),
            MAX_FRAME_LEN
        )));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| RuntimeError::new(format!("frame length {} exceeds u32", payload.len())))?;
    write_all_fd_until(fd, &len.to_be_bytes(), deadline, phase)?;
    write_all_fd_until(fd, payload, deadline, phase)
}

fn write_all_fd_until(
    fd: RawFd,
    mut bytes: &[u8],
    deadline: &CallDeadline<'_>,
    phase: &'static str,
) -> RuntimeResult<()> {
    while !bytes.is_empty() {
        let result = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if result > 0 {
            bytes = &bytes[result as usize..];
            continue;
        }
        if result == 0 {
            return Err(RuntimeError::new(format!(
                "failed to write namespace runtime fd {fd} during {phase}: zero-byte write"
            )));
        }

        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => poll_fd_until(fd, libc::POLLOUT, deadline, phase)?,
            _ => {
                return Err(RuntimeError::new(format!(
                    "failed to write namespace runtime fd {fd} during {phase}: {error}"
                )));
            }
        }
    }

    Ok(())
}

fn read_frame(reader: &mut impl Read) -> RuntimeResult<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    let mut read = 0;
    while read < header.len() {
        match reader.read(&mut header[read..])? {
            0 if read == 0 => return Ok(None),
            0 => {
                return Err(RuntimeError::new(
                    "unexpected EOF while reading frame header",
                ));
            }
            count => read += count,
        }
    }

    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_LEN {
        return Err(RuntimeError::new(format!(
            "frame length {len} exceeds limit {MAX_FRAME_LEN}"
        )));
    }

    let mut payload = vec![0_u8; len];
    let mut read = 0;
    while read < payload.len() {
        match reader.read(&mut payload[read..])? {
            0 => return Err(RuntimeError::new("unexpected EOF while reading frame body")),
            count => read += count,
        }
    }

    Ok(Some(payload))
}

fn read_frame_until(
    fd: RawFd,
    deadline: &CallDeadline<'_>,
    phase: &'static str,
) -> RuntimeResult<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    if !read_exact_fd_until(fd, &mut header, true, deadline, phase)? {
        return Ok(None);
    }

    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_LEN {
        return Err(RuntimeError::new(format!(
            "frame length {len} exceeds limit {MAX_FRAME_LEN}"
        )));
    }

    let mut payload = vec![0_u8; len];
    read_exact_fd_until(fd, &mut payload, false, deadline, phase)?;

    Ok(Some(payload))
}

fn read_exact_fd_until(
    fd: RawFd,
    buffer: &mut [u8],
    allow_empty_eof: bool,
    deadline: &CallDeadline<'_>,
    phase: &'static str,
) -> RuntimeResult<bool> {
    let mut read = 0;
    while read < buffer.len() {
        let result =
            unsafe { libc::read(fd, buffer[read..].as_mut_ptr().cast(), buffer.len() - read) };
        if result > 0 {
            read += result as usize;
            continue;
        }
        if result == 0 {
            if allow_empty_eof && read == 0 {
                return Ok(false);
            }
            return Err(RuntimeError::new(format!(
                "unexpected EOF while reading namespace runtime frame during {phase}"
            )));
        }

        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => poll_fd_until(fd, libc::POLLIN, deadline, phase)?,
            _ => {
                return Err(RuntimeError::new(format!(
                    "failed to read namespace runtime fd {fd} during {phase}: {error}"
                )));
            }
        }
    }

    Ok(true)
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
    use std::io::Cursor;

    #[test]
    fn frame_round_trips() {
        let mut buffer = Vec::new();

        write_frame(&mut buffer, b"hello").unwrap();

        let mut reader = Cursor::new(buffer);
        assert_eq!(read_frame(&mut reader).unwrap(), Some(b"hello".to_vec()));
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn eof_before_frame_header_returns_none() {
        let mut reader = Cursor::new(Vec::new());

        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn partial_frame_header_is_rejected() {
        let mut reader = Cursor::new(vec![0, 0]);

        assert!(
            read_frame(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("frame header")
        );
    }

    #[test]
    fn partial_frame_body_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3_u32.to_be_bytes());
        bytes.extend_from_slice(b"he");
        let mut reader = Cursor::new(bytes);

        assert!(
            read_frame(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("frame body")
        );
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut reader = Cursor::new(((MAX_FRAME_LEN + 1) as u32).to_be_bytes());

        assert!(
            read_frame(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("exceeds limit")
        );
    }

    fn test_ns_runtime() -> NsRuntime {
        NsRuntime {
            executable: open_current_executable().unwrap(),
            idmap: HostIdmap {
                current_uid: 0,
                current_gid: 0,
                subuid: SubidRange { base: 1, count: 1 },
                subgid: SubidRange { base: 1, count: 1 },
            },
            tools: NsTools {
                newuidmap: PathBuf::new(),
                newgidmap: PathBuf::new(),
            },
            next_id: AtomicU64::new(0),
            call_timeout: None,
            _codec: PhantomData,
        }
    }

    #[test]
    fn call_timeout_api_sets_and_clears_timeout() {
        let timeout = Duration::from_secs(30);

        let runtime = test_ns_runtime().with_call_timeout(timeout);
        assert_eq!(runtime.call_timeout(), Some(timeout));

        let runtime = runtime.without_call_timeout();
        assert_eq!(runtime.call_timeout(), None);
    }

    #[test]
    fn namespace_runtime_handle_can_be_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<NsRuntime>();
    }

    #[test]
    fn worker_environment_is_empty() {
        let env = empty_worker_envp();

        assert_eq!(env.len(), 1);
        assert!(env[0].is_null());
    }

    #[test]
    fn current_executable_fd_is_not_stdio_and_is_close_on_exec() {
        let executable = open_current_executable().unwrap();

        assert!(executable.as_raw_fd() > libc::STDERR_FILENO);
        let flags = unsafe { libc::fcntl(executable.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn current_executable_fd_can_be_fexecved_with_empty_environment() {
        let executable = open_current_executable().unwrap();
        let dev_null_read = File::open("/dev/null").unwrap();
        let dev_null_write = File::options().write(true).open("/dev/null").unwrap();
        let arg0 = CString::new("fexecve-smoke-test").unwrap();
        let arg1 = CString::new("--list").unwrap();
        let args = [arg0.as_ptr(), arg1.as_ptr(), std::ptr::null()];
        let env = empty_worker_envp();

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                if libc::dup2(dev_null_read.as_raw_fd(), libc::STDIN_FILENO) < 0 {
                    libc::_exit(127);
                }
                if libc::dup2(dev_null_write.as_raw_fd(), libc::STDOUT_FILENO) < 0 {
                    libc::_exit(127);
                }
                if libc::dup2(dev_null_write.as_raw_fd(), libc::STDERR_FILENO) < 0 {
                    libc::_exit(127);
                }
                libc::fexecve(executable.as_raw_fd(), args.as_ptr(), env.as_ptr());
                libc::_exit(127);
            }
        }

        let status = wait_for_pid(pid).unwrap();
        assert!(
            raw_wait_status_success(status),
            "fexecve smoke test exited with {}",
            raw_wait_status_message(status)
        );
    }

    #[test]
    fn zero_call_deadline_times_out_immediately() {
        let deadline = CallDeadline::new("slow-function", Some(Duration::ZERO));

        let error = deadline.ensure("starting call").unwrap_err();

        assert_eq!(
            error.to_string(),
            "namespace runtime timed out after 0ns while running 'slow-function' during starting call"
        );
    }

    #[test]
    fn fd_frame_round_trips_before_deadline() {
        let Pipe { read, write } = Pipe::new().unwrap();
        set_fd_nonblocking(read.as_raw_fd(), true).unwrap();
        set_fd_nonblocking(write.as_raw_fd(), true).unwrap();
        let deadline = CallDeadline::new("frame-test", Some(Duration::from_secs(1)));

        write_frame_until(write.as_raw_fd(), b"hello", &deadline, "protocol request").unwrap();
        drop(write);

        assert_eq!(
            read_frame_until(read.as_raw_fd(), &deadline, "protocol response").unwrap(),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            read_frame_until(read.as_raw_fd(), &deadline, "protocol response").unwrap(),
            None
        );
    }

    #[test]
    fn fd_frame_read_times_out_when_writer_stays_open() {
        let Pipe { read, write } = Pipe::new().unwrap();
        set_fd_nonblocking(read.as_raw_fd(), true).unwrap();
        let deadline = CallDeadline::new("frame-test", Some(Duration::from_millis(10)));

        let error = read_frame_until(read.as_raw_fd(), &deadline, "protocol response").unwrap_err();
        drop(write);

        assert!(error.to_string().contains(
            "namespace runtime timed out after 10ms while running 'frame-test' during protocol response"
        ));
    }

    #[test]
    fn wait_for_child_kills_and_reaps_after_timeout() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::setpgid(0, 0);
                loop {
                    libc::pause();
                }
            }
        }

        let deadline = CallDeadline::new("sleep", Some(Duration::from_millis(10)));

        let error = wait_for_child(NsChild { pid }, &deadline).unwrap_err();

        assert!(error.to_string().contains(
            "namespace runtime timed out after 10ms while running 'sleep' during child exit"
        ));
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert_eq!(result, -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn ns_function_adapts_typed_function_to_erased_json_call() {
        use crate::runtime::RuntimeFunction;
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Copy)]
        struct TestUppercase;

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TestInput {
            text: String,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TestOutput {
            text: String,
        }

        impl RuntimeFunction for TestUppercase {
            type Input = TestInput;
            type Output = TestOutput;

            fn name(&self) -> &'static str {
                "test-uppercase"
            }

            fn call(&self, input: Self::Input) -> RuntimeResult<Self::Output> {
                Ok(TestOutput {
                    text: input.text.to_uppercase(),
                })
            }
        }

        let function = NsFunction::<JsonCodec>::new(TestUppercase);
        let input = <JsonCodec as WireCodec>::encode(&TestInput {
            text: "abc".to_string(),
        })
        .unwrap();

        let output = function.call_erased(&input).unwrap();
        let output = <JsonCodec as WireCodec>::decode::<TestOutput>(&output).unwrap();

        assert_eq!(function.name(), "test-uppercase");
        assert_eq!(output.text, "ABC");
    }

    #[test]
    fn worker_protocol_is_not_stdout() {
        use crate::runtime::RuntimeFunction;
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Copy)]
        struct NoisyFunction;

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoisyInput;

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoisyOutput {
            value: String,
        }

        impl RuntimeFunction for NoisyFunction {
            type Input = NoisyInput;
            type Output = NoisyOutput;

            fn name(&self) -> &'static str {
                "noisy"
            }

            fn call(&self, _input: Self::Input) -> RuntimeResult<Self::Output> {
                println!("this stdout line must not enter the protocol stream");
                Ok(NoisyOutput {
                    value: "ok".to_string(),
                })
            }
        }

        let mut request_stream = Vec::new();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&ParentToChild::Call {
                id: 7,
                function: "noisy".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&NoisyInput).unwrap(),
        )
        .unwrap();

        let mut reader = Cursor::new(request_stream);
        let mut response_stream = Vec::new();
        run_worker_once::<JsonCodec, _, _>(
            &mut reader,
            &mut response_stream,
            vec![NsFunction::new(NoisyFunction)],
        )
        .unwrap();

        let mut response_reader = Cursor::new(response_stream);
        let response = read_frame(&mut response_reader).unwrap().unwrap();
        assert!(matches!(
            JsonCodec::decode::<ChildToParent>(&response).unwrap(),
            ChildToParent::Ok { id: 7 }
        ));
        let output = read_frame(&mut response_reader).unwrap().unwrap();
        let output = JsonCodec::decode::<NoisyOutput>(&output).unwrap();
        assert_eq!(output.value, "ok");
        assert_eq!(read_frame(&mut response_reader).unwrap(), None);
    }

    #[test]
    fn worker_processes_only_one_call() {
        use crate::runtime::RuntimeFunction;
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Copy)]
        struct OneShotFunction;

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OneShotInput {
            value: String,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OneShotOutput {
            value: String,
        }

        impl RuntimeFunction for OneShotFunction {
            type Input = OneShotInput;
            type Output = OneShotOutput;

            fn name(&self) -> &'static str {
                "one-shot"
            }

            fn call(&self, input: Self::Input) -> RuntimeResult<Self::Output> {
                Ok(OneShotOutput { value: input.value })
            }
        }

        let mut request_stream = Vec::new();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&ParentToChild::Call {
                id: 1,
                function: "one-shot".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&OneShotInput {
                value: "first".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&ParentToChild::Call {
                id: 2,
                function: "one-shot".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&OneShotInput {
                value: "second".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let request_len = request_stream.len();
        let mut reader = Cursor::new(request_stream);
        let mut response_stream = Vec::new();
        run_worker_once::<JsonCodec, _, _>(
            &mut reader,
            &mut response_stream,
            vec![NsFunction::new(OneShotFunction)],
        )
        .unwrap();

        assert!((reader.position() as usize) < request_len);
        let mut response_reader = Cursor::new(response_stream);
        let response = read_frame(&mut response_reader).unwrap().unwrap();
        assert!(matches!(
            JsonCodec::decode::<ChildToParent>(&response).unwrap(),
            ChildToParent::Ok { id: 1 }
        ));
        let output = read_frame(&mut response_reader).unwrap().unwrap();
        let output = JsonCodec::decode::<OneShotOutput>(&output).unwrap();
        assert_eq!(output.value, "first");
        assert_eq!(read_frame(&mut response_reader).unwrap(), None);
    }

    #[test]
    fn worker_returns_error_for_unknown_function() {
        let mut request_stream = Vec::new();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&ParentToChild::Call {
                id: 3,
                function: "missing".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&serde_json::json!({})).unwrap(),
        )
        .unwrap();

        let mut reader = Cursor::new(request_stream);
        let mut response_stream = Vec::new();
        run_worker_once::<JsonCodec, _, _>(&mut reader, &mut response_stream, Vec::new()).unwrap();

        let mut response_reader = Cursor::new(response_stream);
        let response = read_frame(&mut response_reader).unwrap().unwrap();
        match JsonCodec::decode::<ChildToParent>(&response).unwrap() {
            ChildToParent::Err { id, message } => {
                assert_eq!(id, 3);
                assert_eq!(message, "unknown function 'missing'");
            }
            response => panic!("expected error response, got {response:?}"),
        }
        assert_eq!(read_frame(&mut response_reader).unwrap(), None);
    }

    #[test]
    fn worker_returns_error_for_function_panic() {
        use crate::runtime::RuntimeFunction;
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Copy)]
        struct PanicFunction;

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PanicInput;

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PanicOutput;

        impl RuntimeFunction for PanicFunction {
            type Input = PanicInput;
            type Output = PanicOutput;

            fn name(&self) -> &'static str {
                "panic-function"
            }

            fn call(&self, _input: Self::Input) -> RuntimeResult<Self::Output> {
                panic!("boom")
            }
        }

        let mut request_stream = Vec::new();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&ParentToChild::Call {
                id: 4,
                function: "panic-function".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&PanicInput).unwrap(),
        )
        .unwrap();

        let mut reader = Cursor::new(request_stream);
        let mut response_stream = Vec::new();
        run_worker_once::<JsonCodec, _, _>(
            &mut reader,
            &mut response_stream,
            vec![NsFunction::new(PanicFunction)],
        )
        .unwrap();

        let mut response_reader = Cursor::new(response_stream);
        let response = read_frame(&mut response_reader).unwrap().unwrap();
        match JsonCodec::decode::<ChildToParent>(&response).unwrap() {
            ChildToParent::Err { id, message } => {
                assert_eq!(id, 4);
                assert_eq!(message, "function 'panic-function' panicked: boom");
            }
            response => panic!("expected error response, got {response:?}"),
        }
        assert_eq!(read_frame(&mut response_reader).unwrap(), None);
    }

    #[test]
    fn worker_invocation_parser_recognizes_worker_arguments() {
        let invocation = worker_invocation_from_args([
            OsString::from("binary"),
            OsString::from(WORKER_ARG),
            OsString::from(WORKER_PROTOCOL_READ_ARG),
            OsString::from("3"),
            OsString::from(WORKER_PROTOCOL_WRITE_ARG),
            OsString::from("4"),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(invocation.protocol_read_fd, 3);
        assert_eq!(invocation.protocol_write_fd, 4);
    }

    #[test]
    fn worker_invocation_parser_ignores_normal_arguments() {
        assert!(
            worker_invocation_from_args([OsString::from("binary"), OsString::from("--normal")])
                .unwrap()
                .is_none()
        );
    }

    fn subid_owner(username: Option<&str>) -> SubidOwner {
        SubidOwner::new(1001, username.map(str::to_string))
    }

    #[test]
    fn subid_owner_matches_username_and_numeric_uid() {
        let owner = subid_owner(Some("alice"));

        assert!(owner.matches("alice"));
        assert!(owner.matches("1001"));
        assert!(!owner.matches("bob"));
        assert!(!owner.matches("1002"));
    }

    #[test]
    fn subid_owner_without_username_matches_numeric_uid_only() {
        let owner = subid_owner(None);

        assert!(owner.matches("1001"));
        assert!(!owner.matches("alice"));
    }

    #[test]
    fn parser_skips_comments_and_empty_lines() {
        let range = parse_first_subid_range(
            "\n# comment\nalice:100000:65536\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap();

        assert_eq!(
            range,
            SubidRange {
                base: 100_000,
                count: 65_536
            }
        );
    }

    #[test]
    fn parser_accepts_numeric_uid_owner() {
        let range = parse_first_subid_range(
            "1001:100000:65536\n",
            &subid_owner(None),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap();

        assert_eq!(
            range,
            SubidRange {
                base: 100_000,
                count: 65_536
            }
        );
    }

    #[test]
    fn parser_uses_first_matching_entry_across_username_and_uid() {
        let range = parse_first_subid_range(
            "1001:100000:10\nalice:200000:20\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap();

        assert_eq!(
            range,
            SubidRange {
                base: 100_000,
                count: 10
            }
        );
    }

    #[test]
    fn parser_rejects_missing_entry() {
        let error = parse_first_subid_range(
            "bob:100000:10\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("subuid not configured for alice or uid 1001")
        );
    }

    #[test]
    fn parser_rejects_missing_entry_for_uid_only_owner() {
        let error = parse_first_subid_range(
            "bob:100000:10\n",
            &subid_owner(None),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("subuid not configured for uid 1001")
        );
    }

    #[test]
    fn parser_ignores_malformed_non_matching_line() {
        let range = parse_first_subid_range(
            "malformed\nalice:100000:10\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap();

        assert_eq!(
            range,
            SubidRange {
                base: 100_000,
                count: 10
            }
        );
    }

    #[test]
    fn parser_ignores_invalid_number_for_non_matching_owner() {
        let range = parse_first_subid_range(
            "bob:not-a-number:10\nalice:100000:10\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap();

        assert_eq!(
            range,
            SubidRange {
                base: 100_000,
                count: 10
            }
        );
    }

    #[test]
    fn parser_rejects_malformed_matching_line() {
        let error = parse_first_subid_range(
            "alice\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("malformed subuid line 1"));
    }

    #[test]
    fn parser_rejects_invalid_number_for_matching_owner() {
        let error = parse_first_subid_range(
            "alice:not-a-number:10\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid base 'not-a-number'"));
    }

    #[test]
    fn parser_rejects_zero_count() {
        let error = parse_first_subid_range(
            "alice:100000:0\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("zero count"));
    }

    #[test]
    fn parser_rejects_range_overflow() {
        let error = parse_first_subid_range(
            "alice:4294967295:2\n",
            &subid_owner(Some("alice")),
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("overflows u32 range"));
    }
}
