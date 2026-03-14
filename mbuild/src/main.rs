use clap::Parser;
use std::env;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use mbuild::runtime;

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
#[command(about = "mbuild runtime for BuildRequest JSON")]
struct Cli {
    request_file: Option<PathBuf>,
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
    let workspace_root = env::current_dir()
        .map_err(|error| MbuildError::InvalidInput(format!("failed to get current directory: {error}")))?;
    let request_path = cli
        .request_file
        .unwrap_or_else(|| PathBuf::from(".mbuild/request.json"));
    let published =
        runtime::run_workspace_build(&workspace_root, &request_path).map_err(map_runtime_error)?;

    println!("build_key: {}", published.record.build_key);
    println!("object_hash: {}", published.record.object_hash);
    println!("object_path: {}", published.object_path.display());
    Ok(())
}

fn map_runtime_error(error: runtime::RuntimeError) -> MbuildError {
    match error {
        runtime::RuntimeError::InvalidRequest(_)
        | runtime::RuntimeError::UnknownBuilder(_)
        | runtime::RuntimeError::RecipeLoad(_) => MbuildError::InvalidInput(error.to_string()),
        runtime::RuntimeError::Build(_) | runtime::RuntimeError::Store(_) => {
            MbuildError::BuildFailed(error.to_string())
        }
    }
}
