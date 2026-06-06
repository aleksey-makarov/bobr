//! Namespace runtime implementation.
//!
//! [`crate::runtime_ns::NsRuntime`] executes typed runtime functions in a long-lived child process
//! that enters a Linux user namespace before starting the worker loop. Calls are
//! marshalled over length-prefixed frames using a [`crate::runtime_ns::WireCodec`].
//!
//! The parent side constructs [`crate::runtime_ns::NsRuntime`]. The child side must call
//! [`crate::runtime_ns::run_worker`] with a registry of [`crate::runtime_ns::NsFunction`] values when the current
//! executable is launched in worker mode.

use crate::runtime::{Runtime, RuntimeError, RuntimeFunction, RuntimeResult};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::thread::{self, JoinHandle};

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
const WORKER_ARG: &str = "--ns-runtime-worker";
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
/// using this runtime must route the worker invocation to [`run_worker`].
pub struct NsRuntime<C = JsonCodec> {
    child: Option<NsChild>,
    stdin: Option<BufWriter<File>>,
    stdout: BufReader<File>,
    stderr_forwarder: Option<JoinHandle<()>>,
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
    /// The application must route the worker invocation to [`run_worker`], for
    /// example by checking the worker command-line flag before running normal
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
            drop(launch.stdin);
            drop(launch.stdout);
            let _ = launch.stderr_forwarder.join();
            return Err(error);
        }

        Ok(Self {
            child: Some(launch.child),
            stdin: Some(launch.stdin),
            stdout: launch.stdout,
            stderr_forwarder: Some(launch.stderr_forwarder),
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
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| RuntimeError::new("namespace runtime stdin is closed"))?;
        let request = C::encode(&request)?;
        write_frame(stdin, &request)?;
        write_frame(stdin, &input)?;
        stdin.flush()?;

        let response = read_frame(&mut self.stdout)?
            .ok_or_else(|| RuntimeError::new("namespace runtime exited without response"))?;
        match C::decode::<ChildToParent>(&response)? {
            ChildToParent::Ok { id: response_id } if response_id == id => {
                read_frame(&mut self.stdout)?.ok_or_else(|| {
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
        if let Some(mut stdin) = self.stdin.take() {
            let _ = write_frame(&mut stdin, &self.shutdown_request);
            let _ = stdin.flush();
        }
        if let Some(child) = self.child.take() {
            let _ = wait_for_child(child);
        }
        if let Some(forwarder) = self.stderr_forwarder.take() {
            let _ = forwarder.join();
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
    call: Box<dyn Fn(&[u8]) -> RuntimeResult<Vec<u8>> + Send + Sync + 'static>,
    _codec: PhantomData<C>,
}

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

/// Run the namespace worker loop.
///
/// The worker reads framed requests from standard input, dispatches calls by
/// function name through `functions`, and writes framed responses to standard
/// output. The parent side expects the current executable to enter this worker
/// path when started by [`NsRuntime`].
///
/// `functions` must contain at most one entry for each function name.
pub fn run_worker<C>(functions: Vec<NsFunction<C>>) -> RuntimeResult<()>
where
    C: WireCodec,
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

    let stdin = std::io::stdin();
    let mut stdin = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout().lock();

    while let Some(request) = read_frame(&mut stdin)? {
        match C::decode::<ParentToChild>(&request)? {
            ParentToChild::Call { id, function } => {
                let input = read_frame(&mut stdin)?.ok_or_else(|| {
                    RuntimeError::new(format!("missing input frame for call id {id}"))
                })?;
                match registry.get(&function) {
                    Some(function) => match function.call_erased(&input) {
                        Ok(output) => {
                            let response = C::encode(&ChildToParent::Ok { id })?;
                            write_frame(&mut stdout, &response)?;
                            write_frame(&mut stdout, &output)?;
                        }
                        Err(error) => {
                            let response = C::encode(&ChildToParent::Err {
                                id,
                                message: error.to_string(),
                            })?;
                            write_frame(&mut stdout, &response)?;
                        }
                    },
                    None => {
                        let response = C::encode(&ChildToParent::Err {
                            id,
                            message: format!("unknown function '{function}'"),
                        })?;
                        write_frame(&mut stdout, &response)?;
                    }
                }
                stdout.flush()?;
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
    stdin: BufWriter<File>,
    stdout: BufReader<File>,
    stderr_forwarder: JoinHandle<()>,
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
    let protocol_stdin = Pipe::new()?;
    let protocol_stdout = Pipe::new()?;
    let protocol_stderr = Pipe::new()?;
    let userns_ready = Pipe::new()?;
    let idmap_ready = Pipe::new()?;
    let executable = path_cstring(executable)?;
    let arg0 = executable.clone();
    let worker_arg = CString::new(WORKER_ARG).unwrap();
    let args = [arg0, worker_arg];
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
            protocol_stdin.read_raw(),
            protocol_stdout.write_raw(),
            protocol_stderr.write_raw(),
            userns_ready.write_raw(),
            idmap_ready.read_raw(),
        );
    }

    let Pipe {
        read: protocol_stdin_read,
        write: protocol_stdin_write,
    } = protocol_stdin;
    let Pipe {
        read: protocol_stdout_read,
        write: protocol_stdout_write,
    } = protocol_stdout;
    let Pipe {
        read: protocol_stderr_read,
        write: protocol_stderr_write,
    } = protocol_stderr;
    let Pipe {
        read: userns_ready_read,
        write: userns_ready_write,
    } = userns_ready;
    let Pipe {
        read: idmap_ready_read,
        write: idmap_ready_write,
    } = idmap_ready;

    drop(protocol_stdin_read);
    drop(protocol_stdout_write);
    drop(protocol_stderr_write);
    drop(userns_ready_write);
    drop(idmap_ready_read);

    Ok(NsLaunch {
        child: NsChild { pid },
        handshake: NsHandshake {
            userns_ready_read,
            idmap_ready_write,
        },
        stdin: BufWriter::new(File::from(protocol_stdin_write)),
        stdout: BufReader::new(File::from(protocol_stdout_read)),
        stderr_forwarder: spawn_stderr_forwarder(File::from(protocol_stderr_read)),
    })
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
    stdin_read: RawFd,
    stdout_write: RawFd,
    stderr_write: RawFd,
    userns_ready_write: RawFd,
    idmap_ready_read: RawFd,
) -> ! {
    if !child_setup_stdio(stdin_read, stdout_write, stderr_write) {
        unsafe { libc::_exit(127) };
    }
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        child_write_stderr(b"failed to unshare user namespace\n");
        unsafe { libc::_exit(127) };
    }
    if write_handshake_byte(userns_ready_write).is_err() {
        child_write_stderr(b"failed to signal user namespace readiness\n");
        unsafe { libc::_exit(127) };
    }
    if !child_read_handshake_byte(idmap_ready_read) {
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
        child_close_fd(userns_ready_write);
        child_close_fd(idmap_ready_read);
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

fn spawn_stderr_forwarder(mut stderr: File) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut parent_stderr = std::io::stderr().lock();
        let _ = io::copy(&mut stderr, &mut parent_stderr);
    })
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
