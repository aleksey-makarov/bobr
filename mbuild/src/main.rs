use clap::Parser;
use std::env;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use mbuild::store_interpreter::{self, StoreOutcome};

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
#[command(about = "mbuild runtime for Nickel STORE recipes")]
struct Cli {
    recipe_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class(), error);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> MResult<()> {
    let workspace_root = env::current_dir().map_err(|error| {
        MbuildError::InvalidInput(format!("failed to get current directory: {error}"))
    })?;
    let recipe_path = cli
        .recipe_file
        .unwrap_or_else(|| PathBuf::from(".mbuild/recipe.ncl"));
    match store_interpreter::run_store_recipe_in_workspace(&workspace_root, &recipe_path)
        .map_err(map_runtime_error)?
    {
        StoreOutcome::Build(published) => {
            println!("build_key: {}", published.record.build_key);
            println!("object_hash: {}", published.record.object_hash);
            println!("object_path: {}", published.object_path.display());
        }
        StoreOutcome::Unit => println!("()"),
    }
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
