use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use mbuild::RecipeEnvelope;
use mbuild::recipe_runtime;
use mbuild_core::CancellationToken;
use tracing_subscriber::EnvFilter;

type MResult<T> = Result<T, MbuildError>;

#[derive(Debug)]
enum MbuildError {
    InvalidInput(String),
    Cancelled(String),
    BuildFailed(String),
}

impl MbuildError {
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

impl fmt::Display for MbuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

fn main() -> ExitCode {
    if let Some(exit_code) = run_runtime_worker_if_requested() {
        return exit_code;
    }

    init_tracing();

    let cancellation = CancellationToken::new();
    signal::install_handlers(cancellation.clone());
    let result = build(cancellation);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class(), error);
            if matches!(error, MbuildError::Cancelled(_)) {
                ExitCode::from(130)
            } else {
                ExitCode::from(1)
            }
        }
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
    let mut functions = mbuild_builder::runtime_functions();
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

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .without_time()
        .try_init();
}

fn build(cancellation: CancellationToken) -> MResult<()> {
    let recipe_file = recipe_file_from_args()?;
    let recipe_bytes = read_recipe_bytes(recipe_file.as_ref())?;
    let envelope = RecipeEnvelope::parse_json(&recipe_bytes).map_err(map_runtime_error)?;

    let build =
        recipe_runtime::run_recipe_envelope(envelope, cancellation).map_err(map_runtime_error)?;
    let rendered = recipe_runtime::render_object_as_json(&build).map_err(map_runtime_error)?;
    print!("{rendered}");
    Ok(())
}

fn recipe_file_from_args() -> MResult<Option<PathBuf>> {
    let mut args = env::args_os();
    let _program = args.next();
    let Some(recipe_file) = args.next() else {
        return Ok(None);
    };
    if let Some(extra) = args.next() {
        return Err(MbuildError::InvalidInput(format!(
            "unexpected argument '{}'; usage: mbuild [recipe.json]",
            extra.to_string_lossy()
        )));
    }
    Ok(Some(PathBuf::from(recipe_file)))
}

fn read_recipe_bytes(recipe_file: Option<&PathBuf>) -> MResult<Vec<u8>> {
    match recipe_file {
        Some(path) => fs::read(path).map_err(|error| {
            MbuildError::InvalidInput(format!(
                "failed to read recipe file '{}': {error}",
                path.display()
            ))
        }),
        None => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes).map_err(|error| {
                MbuildError::InvalidInput(format!("failed to read recipe JSON from stdin: {error}"))
            })?;
            Ok(bytes)
        }
    }
}

fn map_runtime_error(error: mbuild::RuntimeError) -> MbuildError {
    match error {
        mbuild::RuntimeError::InvalidRequest(_)
        | mbuild::RuntimeError::UnknownBuilder(_)
        | mbuild::RuntimeError::RecipeLoad(_) => MbuildError::InvalidInput(error.to_string()),
        mbuild::RuntimeError::Cancelled(_) => MbuildError::Cancelled(error.to_string()),
        mbuild::RuntimeError::Build(_) | mbuild::RuntimeError::Store(_) => {
            MbuildError::BuildFailed(error.to_string())
        }
    }
}

#[cfg(unix)]
mod signal {
    use mbuild_core::CancellationToken;
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

    pub fn install_handlers(cancellation: CancellationToken) {
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
    use mbuild_core::CancellationToken;

    pub fn install_handlers(_cancellation: CancellationToken) {}
}
