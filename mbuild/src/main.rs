use clap::{Args, Parser, Subcommand};
use std::env;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use mbuild::recipe_runtime::{self, BuildRunOptions};

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

#[derive(Args, Debug, Default)]
struct BuildCli {
    #[arg(long, help = "suppress live build progress on stderr")]
    quiet: bool,

    #[arg(short = 'j', long = "jobs", default_value_t = default_jobs())]
    jobs: usize,

    recipe_file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(about = "build a JSON recipe graph")]
    Build(BuildCli),
}

#[derive(Parser, Debug)]
#[command(name = "mbuild")]
#[command(about = "mbuild runtime for JSON recipe graphs")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    build: BuildCli,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Build(build_cli)) => build(build_cli),
        None => build(cli.build),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class(), error);
            ExitCode::from(1)
        }
    }
}

fn build(cli: BuildCli) -> MResult<()> {
    let workspace_root = env::current_dir().map_err(|error| {
        MbuildError::InvalidInput(format!("failed to get current directory: {error}"))
    })?;
    let recipe_path = cli
        .recipe_file
        .unwrap_or_else(|| PathBuf::from(".mbuild/recipe.json"));
    let build = recipe_runtime::run_recipe_json_in_workspace_with_options(
        &workspace_root,
        &recipe_path,
        BuildRunOptions {
            emit_progress: !cli.quiet,
            jobs: cli.jobs,
        },
    )
    .map_err(map_runtime_error)?;
    let rendered = recipe_runtime::render_build_as_json(&build).map_err(map_runtime_error)?;
    print!("{rendered}");
    Ok(())
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
