//! Namespace runtime implementation.
//!
//! [`crate::runtime_ns::NsRuntime`] executes typed runtime functions in a long-lived child process
//! that enters a Linux user namespace before starting the worker loop. Calls are
//! marshalled over length-prefixed frames using a [`crate::runtime_ns::WireCodec`].
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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
const WORKER_ARG: &str = "--ns-runtime-worker";
const WORKER_PROTOCOL_READ_ARG: &str = "--ns-runtime-protocol-read-fd";
const WORKER_PROTOCOL_WRITE_ARG: &str = "--ns-runtime-protocol-write-fd";
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
/// `NsRuntime` owns a single long-lived worker process. Each call to
/// [`Runtime::run`] encodes the typed input, sends a framed request to that
/// worker, waits for a framed response, and decodes the typed output.
///
/// The worker process is started from the current executable, so applications
/// using this runtime must route the worker invocation through
/// [`worker_invocation_from_env`] and [`run_worker`].
///
/// Worker standard input, output, and error are redirected to `/dev/null`.
/// Runtime functions must return data through their typed result, not through
/// process stdio. If a runtime function starts subprocesses and needs their
/// output for diagnostics or results, the function implementation must capture
/// that output explicitly, for example by using `Command::output` or piped
/// `Stdio`; inherited subprocess stdout and stderr are discarded.
pub struct NsRuntime<C = JsonCodec> {
    child: Option<NsChild>,
    protocol_writer: Option<BufWriter<File>>,
    protocol_reader: BufReader<File>,
    shutdown_request: Vec<u8>,
    next_id: u64,
    _codec: PhantomData<C>,
}

impl NsRuntime<JsonCodec> {
    /// Start a namespace runtime using the default [`JsonCodec`].
    ///
    /// This is a convenience constructor for applications that use JSON for
    /// both control messages and function payloads. It is equivalent to
    /// [`NsRuntime::<JsonCodec>::new_with_codec`].
    ///
    /// Construction starts the current executable as a worker process, waits
    /// for the child to enter a Linux user namespace, configures the uid/gid
    /// maps through `newuidmap` and `newgidmap`, and keeps the child alive for
    /// later [`Runtime::run`] calls.
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
    /// This constructor performs all namespace setup immediately. It can fail
    /// if the current executable cannot be resolved, `/etc/subuid` or
    /// `/etc/subgid` does not contain a usable range for the current user,
    /// `newuidmap` or `newgidmap` is missing, user namespaces are unavailable,
    /// or the child process exits during setup.
    pub fn new_with_codec() -> RuntimeResult<Self> {
        let executable = env::current_exe().map_err(|error| {
            RuntimeError::new(format!("failed to locate current executable: {error}"))
        })?;
        let shutdown_request = C::encode(&ParentToChild::Shutdown)
            .map_err(|error| RuntimeError::new(format!("failed to encode shutdown: {error}")))?;
        let idmap = HostIdmap::from_host_environment()?;
        let tools = NsTools::resolve()?;
        let launch = fork_ns_worker(&executable)?;

        if let Err(error) =
            complete_namespace_setup(&launch.child, &launch.handshake, &tools, &idmap)
        {
            terminate_child(launch.child);
            drop(launch.handshake);
            drop(launch.protocol_writer);
            drop(launch.protocol_reader);
            return Err(error);
        }

        Ok(Self {
            child: Some(launch.child),
            protocol_writer: Some(launch.protocol_writer),
            protocol_reader: launch.protocol_reader,
            shutdown_request,
            next_id: 0,
            _codec: PhantomData,
        })
    }
}

impl<C> Runtime for NsRuntime<C>
where
    C: WireCodec,
{
    fn run<F>(&mut self, function: &F, input: F::Input) -> Result<F::Output, RuntimeError>
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
    fn call_erased(&mut self, function_name: &str, input: Vec<u8>) -> RuntimeResult<Vec<u8>> {
        let id = self.next_id;
        self.next_id += 1;

        let request = ParentToChild::Call {
            id,
            function: function_name.to_string(),
        };
        let protocol_writer = self
            .protocol_writer
            .as_mut()
            .ok_or_else(|| RuntimeError::new("namespace runtime protocol writer is closed"))?;
        let request = C::encode(&request)?;
        write_frame(protocol_writer, &request)?;
        write_frame(protocol_writer, &input)?;
        protocol_writer.flush()?;

        let response = read_frame(&mut self.protocol_reader)?
            .ok_or_else(|| RuntimeError::new("namespace runtime exited without response"))?;
        match C::decode::<ChildToParent>(&response)? {
            ChildToParent::Ok { id: response_id } if response_id == id => {
                read_frame(&mut self.protocol_reader)?.ok_or_else(|| {
                    RuntimeError::new("namespace runtime exited without output frame")
                })
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
}

impl<C> Drop for NsRuntime<C> {
    fn drop(&mut self) {
        if let Some(mut protocol_writer) = self.protocol_writer.take() {
            let _ = write_frame(&mut protocol_writer, &self.shutdown_request);
            let _ = protocol_writer.flush();
        }
        if let Some(child) = self.child.take() {
            let _ = wait_for_child(child);
        }
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
    _codec: PhantomData<C>,
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
            "unexpected namespace runtime worker argument '{}'",
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
            "expected namespace runtime worker argument {expected}, got '{}'",
            actual.to_string_lossy()
        )))
    }
}

fn next_worker_arg(
    args: &mut impl Iterator<Item = OsString>,
    expected: &'static str,
) -> RuntimeResult<OsString> {
    args.next().ok_or_else(|| {
        RuntimeError::new(format!(
            "missing namespace runtime worker argument {expected}"
        ))
    })
}

fn parse_worker_fd_arg(value: OsString) -> RuntimeResult<RawFd> {
    let value = value.into_string().map_err(|value| {
        RuntimeError::new(format!(
            "namespace runtime worker fd '{}' is not valid UTF-8",
            value.to_string_lossy()
        ))
    })?;
    value.parse::<RawFd>().map_err(|error| {
        RuntimeError::new(format!(
            "namespace runtime worker fd '{value}' is not an integer: {error}"
        ))
    })
}

/// Run the namespace worker loop.
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
/// `functions` must contain at most one entry for each function name.
pub fn run_worker<C>(
    invocation: WorkerInvocation,
    functions: Vec<NsFunction<C>>,
) -> RuntimeResult<()>
where
    C: WireCodec,
{
    let mut protocol = open_worker_protocol(invocation)?;
    run_worker_loop::<C, _, _>(&mut protocol.reader, &mut protocol.writer, functions)
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

fn run_worker_loop<C, R, W>(
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

    while let Some(request) = read_frame(reader)? {
        match C::decode::<ParentToChild>(&request)? {
            ParentToChild::Call { id, function } => {
                let input = read_frame(reader)?.ok_or_else(|| {
                    RuntimeError::new(format!("missing input frame for call id {id}"))
                })?;
                match registry.get(&function) {
                    Some(function) => match function.call_erased(&input) {
                        Ok(output) => {
                            let response = C::encode(&ChildToParent::Ok { id })?;
                            write_frame(writer, &response)?;
                            write_frame(writer, &output)?;
                        }
                        Err(error) => {
                            let response = C::encode(&ChildToParent::Err {
                                id,
                                message: error.to_string(),
                            })?;
                            write_frame(writer, &response)?;
                        }
                    },
                    None => {
                        let response = C::encode(&ChildToParent::Err {
                            id,
                            message: format!("unknown function '{function}'"),
                        })?;
                        write_frame(writer, &response)?;
                    }
                }
                writer.flush()?;
            }
            ParentToChild::Shutdown => break,
        }
    }

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ParentToChild {
    Call { id: u64, function: String },
    Shutdown,
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

struct NsLaunch {
    child: NsChild,
    handshake: NsHandshake,
    protocol_writer: BufWriter<File>,
    protocol_reader: BufReader<File>,
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
        let username = current_username(current_uid)?;
        let subuid = read_first_subid_range(Path::new(SUBUID_PATH), &username, SubidKind::Uid)?;
        let subgid = read_first_subid_range(Path::new(SUBGID_PATH), &username, SubidKind::Gid)?;

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

fn fork_ns_worker(executable: &Path) -> RuntimeResult<NsLaunch> {
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
    let executable = path_cstring(executable)?;
    let arg0 = executable.clone();
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
            &executable,
            &arg_ptrs,
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

    Ok(NsLaunch {
        child: NsChild { pid },
        handshake: NsHandshake {
            userns_ready_read,
            idmap_ready_write,
        },
        protocol_writer: BufWriter::new(File::from(protocol_request_write)),
        protocol_reader: BufReader::new(File::from(protocol_response_read)),
    })
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
) -> RuntimeResult<()> {
    wait_for_child_userns(handshake.userns_ready_read.as_raw_fd())?;
    configure_id_maps(tools, child.pid_u32(), idmap)?;
    signal_child_ready(handshake.idmap_ready_write.as_raw_fd())
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
    executable: &CString,
    args: &[*const libc::c_char],
    fds: ChildExecFds,
) -> ! {
    if !child_setup_stdio(fds.stdin_read, fds.stdout_write, fds.stderr_write) {
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
        libc::execv(executable.as_ptr(), args.as_ptr());
    }
    child_write_stderr(b"failed to exec namespace runtime worker\n");
    unsafe { libc::_exit(127) };
}

// The helpers below are used by child_exec_ns_worker() before execv(). Keep
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

fn wait_for_child_userns(fd: RawFd) -> RuntimeResult<()> {
    read_handshake_byte(fd, "child user namespace setup")
}

fn signal_child_ready(fd: RawFd) -> RuntimeResult<()> {
    write_handshake_byte(fd).map_err(|error| {
        RuntimeError::new(format!(
            "failed to signal namespace runtime readiness: {error}"
        ))
    })
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

fn read_handshake_byte(fd: RawFd, label: &str) -> RuntimeResult<()> {
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
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(RuntimeError::new(format!(
                "failed to read namespace runtime {label} pipe: {error}"
            )));
        }
    }
}

fn configure_id_maps(tools: &NsTools, pid: u32, idmap: &HostIdmap) -> RuntimeResult<()> {
    run_map_command(
        &tools.newuidmap,
        pid,
        [
            ("0", idmap.current_uid, 1),
            ("1", idmap.subuid.base, idmap.subuid.count),
        ],
    )?;
    write_setgroups_deny(pid)?;
    run_map_command(
        &tools.newgidmap,
        pid,
        [
            ("0", idmap.current_gid, 1),
            ("1", idmap.subgid.base, idmap.subgid.count),
        ],
    )
}

fn run_map_command<const N: usize>(
    program: &Path,
    pid: u32,
    ranges: [(&str, u32, u32); N],
) -> RuntimeResult<()> {
    let mut command = Command::new(program);
    command.arg(pid.to_string());
    for (inside, outside, count) in ranges {
        command
            .arg(inside)
            .arg(outside.to_string())
            .arg(count.to_string());
    }
    let output = command.output().map_err(|error| {
        RuntimeError::new(format!("failed to run '{}': {error}", program.display()))
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(RuntimeError::new(format!(
            "'{}' failed with {}{}",
            program.display(),
            status_message(output.status),
            command_context(&output.stderr)
        )))
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

fn terminate_child(child: NsChild) {
    unsafe {
        libc::kill(child.pid, libc::SIGKILL);
    }
    let _ = wait_for_pid(child.pid);
}

fn wait_for_child(child: NsChild) -> RuntimeResult<()> {
    let status = wait_for_pid(child.pid)?;
    if raw_wait_status_success(status) {
        Ok(())
    } else {
        Err(RuntimeError::new(format!(
            "namespace runtime exited with {}",
            raw_wait_status_message(status)
        )))
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

fn current_username(current_uid: u32) -> RuntimeResult<String> {
    let mut password = unsafe { std::mem::zeroed::<libc::passwd>() };
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; passwd_buffer_size()];
    let status = unsafe {
        libc::getpwuid_r(
            current_uid,
            &mut password,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(RuntimeError::new(format!(
            "failed to look up passwd entry for euid {current_uid}: {}",
            io::Error::from_raw_os_error(status)
        )));
    }
    if result.is_null() {
        return Err(RuntimeError::new(format!(
            "current euid {current_uid} has no passwd entry"
        )));
    }
    let name = unsafe { CStr::from_ptr(password.pw_name) };
    name.to_str().map(str::to_string).map_err(|error| {
        RuntimeError::new(format!(
            "passwd entry name for euid {current_uid} is not valid UTF-8: {error}"
        ))
    })
}

fn passwd_buffer_size() -> usize {
    let size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    if size > 0 { size as usize } else { 16 * 1024 }
}

fn read_first_subid_range(
    path: &Path,
    username: &str,
    kind: SubidKind,
) -> RuntimeResult<SubidRange> {
    let content = fs::read_to_string(path).map_err(|error| {
        RuntimeError::new(format!(
            "failed to read {} file '{}': {error}",
            kind.subid_name(),
            path.display()
        ))
    })?;
    parse_first_subid_range(&content, username, kind, &path.display().to_string())
}

fn parse_first_subid_range(
    content: &str,
    username: &str,
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

        let parts = line.split(':').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(RuntimeError::new(format!(
                "malformed {} line {line_number} in {source}: expected <username>:<base>:<count>",
                kind.subid_name()
            )));
        }

        let base = parse_u32_field(parts[1], "base", kind, source, line_number)?;
        let count = parse_u32_field(parts[2], "count", kind, source, line_number)?;
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

        if parts[0] == username && first_match.is_none() {
            first_match = Some(SubidRange { base, count });
        }
    }

    first_match.ok_or_else(|| {
        RuntimeError::new(format!(
            "{} not configured for user {username} in {source}",
            kind.subid_name()
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

fn path_cstring(path: &Path) -> RuntimeResult<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        RuntimeError::new(format!(
            "path '{}' contains an interior NUL byte",
            path.display()
        ))
    })
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
        write_frame(
            &mut request_stream,
            &JsonCodec::encode(&ParentToChild::Shutdown).unwrap(),
        )
        .unwrap();

        let mut reader = Cursor::new(request_stream);
        let mut response_stream = Vec::new();
        run_worker_loop::<JsonCodec, _, _>(
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

    #[test]
    fn parser_skips_comments_and_empty_lines() {
        let range = parse_first_subid_range(
            "\n# comment\nalice:100000:65536\n",
            "alice",
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
    fn parser_uses_first_matching_entry() {
        let range = parse_first_subid_range(
            "alice:100000:10\nalice:200000:20\n",
            "alice",
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
        let error =
            parse_first_subid_range("bob:100000:10\n", "alice", SubidKind::Uid, "/etc/subuid")
                .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("subuid not configured for user alice")
        );
    }

    #[test]
    fn parser_rejects_malformed_line_anywhere() {
        let error = parse_first_subid_range(
            "alice:100000:10\nmalformed\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("malformed subuid line 2"));
    }

    #[test]
    fn parser_rejects_invalid_number_anywhere() {
        let error = parse_first_subid_range(
            "alice:100000:10\nbob:not-a-number:10\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid base 'not-a-number'"));
    }

    #[test]
    fn parser_rejects_zero_count() {
        let error =
            parse_first_subid_range("alice:100000:0\n", "alice", SubidKind::Uid, "/etc/subuid")
                .unwrap_err();

        assert!(error.to_string().contains("zero count"));
    }

    #[test]
    fn parser_rejects_range_overflow() {
        let error = parse_first_subid_range(
            "alice:4294967295:2\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("overflows u32 range"));
    }
}
