use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use mbuild::recipe_runtime::{self, BuildRunOptions};
use mbuild::{RecipeEnvelope, RecipeOptions};
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
    #[arg(long, action = ArgAction::SetTrue, help = "suppress live build progress on stderr")]
    quiet: bool,

    #[arg(short = 'j', long = "jobs")]
    jobs: Option<usize>,

    recipe_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    init_tracing();

    let matches = Cli::command().get_matches();
    let quiet_from_cli = matches.value_source("quiet") == Some(ValueSource::CommandLine);
    let jobs_from_cli = matches.value_source("jobs") == Some(ValueSource::CommandLine);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let cancellation = CancellationToken::new();
    signal::install_handlers(cancellation.clone());
    let result = build(cli, quiet_from_cli, jobs_from_cli, cancellation);

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

fn build(
    cli: Cli,
    quiet_from_cli: bool,
    jobs_from_cli: bool,
    cancellation: CancellationToken,
) -> MResult<()> {
    let recipe_bytes = read_recipe_bytes(cli.recipe_file.as_ref())?;
    let envelope = RecipeEnvelope::parse_json(&recipe_bytes).map_err(map_runtime_error)?;

    let store_path = envelope.paths.store.clone();
    validate_existing_dir(&store_path, "store path")?;

    env::set_current_dir(&store_path).map_err(|error| {
        MbuildError::InvalidInput(format!(
            "failed to change directory to store root '{}': {error}",
            store_path.display()
        ))
    })?;

    let options = resolve_build_options(
        &envelope.options,
        quiet_from_cli.then_some(cli.quiet),
        if jobs_from_cli { cli.jobs } else { None },
        cancellation,
    )?;
    let build = recipe_runtime::run_recipe_request_in_store_with_options(
        &envelope.paths,
        envelope.request,
        options,
    )
    .map_err(map_runtime_error)?;
    let rendered = recipe_runtime::render_object_as_json(&build).map_err(map_runtime_error)?;
    print!("{rendered}");
    Ok(())
}

fn validate_existing_dir(path: &std::path::Path, label: &str) -> MResult<()> {
    let metadata = fs::metadata(path).map_err(|error| {
        MbuildError::InvalidInput(format!(
            "{label} '{}' does not exist or is not accessible: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(MbuildError::InvalidInput(format!(
            "{label} '{}' is not a directory",
            path.display()
        )));
    }
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

fn resolve_build_options(
    recipe_options: &RecipeOptions,
    quiet_from_cli: Option<bool>,
    jobs_from_cli: Option<usize>,
    cancellation: CancellationToken,
) -> MResult<BuildRunOptions> {
    let quiet = quiet_from_cli.or(recipe_options.quiet).unwrap_or(false);
    let jobs = jobs_from_cli
        .or(recipe_options.jobs)
        .unwrap_or_else(default_jobs);
    if jobs == 0 {
        return Err(MbuildError::InvalidInput(
            "--jobs and recipe options.jobs must be greater than zero".to_string(),
        ));
    }
    Ok(BuildRunOptions {
        emit_progress: !quiet,
        jobs,
        cancellation,
    })
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

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}
