//! Command-line entry point for unified acquisition and build realization.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use bobr::{BuildInfo, Request, realize};
use bobr_core::CancellationToken;

type MResult<T> = Result<T, BobrError>;

#[derive(Debug)]
enum BobrError {
    InvalidInput(String),
    Cancelled(String),
    BuildFailed(String),
}

impl BobrError {
    fn class(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "invalid-input",
            Self::Cancelled(_) => "cancelled",
            Self::BuildFailed(_) => "build-failed",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Cancelled(message) | Self::BuildFailed(message) => {
                message
            }
        }
    }
}

impl fmt::Display for BobrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

fn main() -> ExitCode {
    if let Some(exit_code) = run_runtime_worker_if_requested() {
        return exit_code;
    }

    let result = run();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class(), error);
            if matches!(error, BobrError::Cancelled(_)) {
                ExitCode::from(130)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Invocation {
    Build(Option<PathBuf>),
    BuildInfo,
    Version,
}

fn run() -> MResult<()> {
    match invocation_from_args()? {
        Invocation::Build(request_file) => {
            let cancellation = CancellationToken::new();
            signal::install_handlers(cancellation.clone());
            build(request_file, cancellation)
        }
        Invocation::BuildInfo => print_build_info(),
        Invocation::Version => print_version(),
    }
}

fn run_runtime_worker_if_requested() -> Option<ExitCode> {
    match bobr_runtime::runtime_ns::worker_invocation_from_env() {
        Ok(Some(invocation)) => {
            let result = bobr_runtime::runtime_ns::run_worker(invocation, runtime_functions());
            Some(runtime_worker_exit_code(result))
        }
        Ok(None) => None,
        Err(error) => {
            eprintln!("error[bobr-runtime-worker]: {error}");
            Some(ExitCode::FAILURE)
        }
    }
}

fn runtime_functions() -> Vec<bobr_runtime::runtime_ns::NsFunction> {
    let mut functions = bobr_store::runtime_functions();
    functions.extend(bobr_repo::runtime_functions());
    functions.extend(bobr_builder::runtime_functions());
    functions.extend(bobr_sandbox::runtime_functions());
    functions
}

fn runtime_worker_exit_code(result: bobr_runtime::runtime::RuntimeResult<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[bobr-runtime-worker]: {error}");
            ExitCode::FAILURE
        }
    }
}

fn build(request_file: Option<PathBuf>, cancellation: CancellationToken) -> MResult<()> {
    let request_bytes = read_request_bytes(request_file.as_ref())?;
    let request = Request::parse_json(&request_bytes).map_err(map_execution_error)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            BobrError::BuildFailed(format!("failed to start Tokio runtime: {error}"))
        })?;
    let goals = runtime
        .block_on(realize(request, cancellation))
        .map_err(map_execution_error)?;
    if goals.len() == 1 {
        println!("{}", goals[0].object_hash);
    } else {
        println!(
            "{}",
            serde_json::to_string(&goals).map_err(|error| {
                BobrError::BuildFailed(format!("failed to encode goal results: {error}"))
            })?
        );
    }
    Ok(())
}

/// Prints the build's own version and the request schema it accepts.
///
/// The two answer different questions: the version identifies this build in a
/// report, while the schema is what decides whether a given recipe layer can
/// talk to it at all.
fn print_version() -> MResult<()> {
    let build_info = current_build_info()?;
    println!("{build_info}");
    Ok(())
}

fn print_build_info() -> MResult<()> {
    let build_info = current_build_info()?;
    println!(
        "{}",
        serde_json::to_string(&build_info).map_err(|error| {
            BobrError::BuildFailed(format!("failed to encode build information: {error}"))
        })?
    );
    Ok(())
}

fn current_build_info() -> MResult<BuildInfo> {
    BuildInfo::current().map_err(|error| {
        BobrError::BuildFailed(format!("invalid compile-time build information: {error}"))
    })
}

fn invocation_from_args() -> MResult<Invocation> {
    let mut args = env::args_os();
    let _program = args.next();
    let Some(first) = args.next() else {
        return Ok(Invocation::Build(None));
    };

    if let Some(extra) = args.next() {
        return Err(BobrError::InvalidInput(format!(
            "unexpected argument '{}'; usage: bobr [--version | --build-info | request.json]",
            extra.to_string_lossy()
        )));
    }

    if first == "--version" || first == "-V" {
        Ok(Invocation::Version)
    } else if first == "--build-info" {
        Ok(Invocation::BuildInfo)
    } else {
        Ok(Invocation::Build(Some(PathBuf::from(first))))
    }
}

fn read_request_bytes(request_file: Option<&PathBuf>) -> MResult<Vec<u8>> {
    match request_file {
        Some(path) => fs::read(path).map_err(|error| {
            BobrError::InvalidInput(format!(
                "failed to read request file '{}': {error}",
                path.display()
            ))
        }),
        None => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes).map_err(|error| {
                BobrError::InvalidInput(format!("failed to read request JSON from stdin: {error}"))
            })?;
            Ok(bytes)
        }
    }
}

fn map_execution_error(error: bobr::ExecutionError) -> BobrError {
    match error {
        bobr::ExecutionError::InvalidRequest(_)
        | bobr::ExecutionError::UnknownBuilder(_)
        | bobr::ExecutionError::RequestLoad(_) => BobrError::InvalidInput(error.to_string()),
        bobr::ExecutionError::Cancelled(_) => BobrError::Cancelled(error.to_string()),
        bobr::ExecutionError::Build(_)
        | bobr::ExecutionError::Store(_)
        | bobr::ExecutionError::Run(_) => BobrError::BuildFailed(error.to_string()),
    }
}

#[cfg(unix)]
mod signal {
    use bobr_core::CancellationToken;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn handle_signal(_signal: libc::c_int) {
        if SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst) > 0 {
            unsafe {
                libc::_exit(130);
            }
        }
    }

    pub(crate) fn install_handlers(cancellation: CancellationToken) {
        unsafe {
            libc::signal(
                libc::SIGINT,
                handle_signal as *const () as libc::sighandler_t,
            );
            libc::signal(
                libc::SIGTERM,
                handle_signal as *const () as libc::sighandler_t,
            );
        }
        thread::spawn(move || {
            loop {
                if SIGNAL_COUNT.load(Ordering::SeqCst) > 0 {
                    cancellation.cancel();
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        });
    }
}

#[cfg(not(unix))]
mod signal {
    use bobr_core::CancellationToken;

    pub(crate) fn install_handlers(_cancellation: CancellationToken) {}
}
