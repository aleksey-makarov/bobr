use clap::{ArgAction, Parser};
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

#[derive(Parser, Debug)]
#[command(name = "mbuild")]
#[command(about = "mbuild runtime for JSON recipe graphs")]
struct Cli {
    #[arg(long, action = ArgAction::SetTrue, help = "Suppress live build progress on stderr")]
    quiet: bool,

    #[arg(
        short = 'j',
        long = "jobs",
        help = "Set the maximum number of parallel jobs"
    )]
    jobs: Option<usize>,

    #[arg(long = "store", help = "Use this store root for the build")]
    store: Option<PathBuf>,

    #[arg(help = "Read recipe JSON from this file instead of stdin")]
    recipe_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    init_tracing();

    let cli = Cli::parse();
    let cancellation = CancellationToken::new();
    signal::install_handlers(cancellation.clone());
    let result = build(cli, cancellation);

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

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .without_time()
        .try_init();
}

fn build(cli: Cli, cancellation: CancellationToken) -> MResult<()> {
    mbuild::builders::ensure_registered_builders_valid().map_err(MbuildError::InvalidInput)?;

    let recipe_bytes = read_recipe_bytes(cli.recipe_file.as_ref())?;
    let mut envelope = RecipeEnvelope::parse_json(&recipe_bytes).map_err(map_runtime_error)?;

    if let Some(store) = cli.store {
        envelope.options.store = Some(store);
    }
    if cli.quiet {
        envelope.options.quiet = Some(true);
    }
    if let Some(jobs) = cli.jobs {
        envelope.options.jobs = Some(jobs);
    }

    let build =
        recipe_runtime::run_recipe_envelope(envelope, cancellation).map_err(map_runtime_error)?;
    let rendered = recipe_runtime::render_object_as_json(&build).map_err(map_runtime_error)?;
    print!("{rendered}");
    Ok(())
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
