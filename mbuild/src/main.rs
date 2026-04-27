use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use mbuild::recipe_runtime::{self, BuildRunOptions};
use mbuild::{RecipeEnvelope, RecipeOptions};

type MResult<T> = Result<T, MbuildError>;

#[derive(Debug)]
enum MbuildError {
    InvalidInput(String),
    BuildFailed(String),
}

impl MbuildError {
    fn class(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "invalid-input",
            Self::BuildFailed(_) => "build-failed",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::BuildFailed(message) => message,
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
    let matches = Cli::command().get_matches();
    let quiet_from_cli = matches.value_source("quiet") == Some(ValueSource::CommandLine);
    let jobs_from_cli = matches.value_source("jobs") == Some(ValueSource::CommandLine);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let result = build(cli, quiet_from_cli, jobs_from_cli);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class(), error);
            ExitCode::from(1)
        }
    }
}

fn build(cli: Cli, quiet_from_cli: bool, jobs_from_cli: bool) -> MResult<()> {
    let recipe_bytes = read_recipe_bytes(cli.recipe_file.as_ref())?;
    let envelope = RecipeEnvelope::parse_json(&recipe_bytes).map_err(map_runtime_error)?;

    let store_path = envelope.paths.store.clone();
    validate_existing_dir(&store_path, "store path")?;
    if let Some(local_path) = envelope.paths.local.as_ref() {
        validate_existing_dir(local_path, "local path")?;
    }

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
    )?;
    let build = recipe_runtime::run_recipe_request_in_store_with_options(
        &envelope.paths,
        envelope.request,
        options,
    )
    .map_err(map_runtime_error)?;
    let rendered = recipe_runtime::render_result_as_json(&build).map_err(map_runtime_error)?;
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
    })
}

fn map_runtime_error(error: mbuild::RuntimeError) -> MbuildError {
    match error {
        mbuild::RuntimeError::InvalidRequest(_)
        | mbuild::RuntimeError::UnknownBuilder(_)
        | mbuild::RuntimeError::RecipeLoad(_) => MbuildError::InvalidInput(error.to_string()),
        mbuild::RuntimeError::Build(_) | mbuild::RuntimeError::Store(_) => {
            MbuildError::BuildFailed(error.to_string())
        }
    }
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}
