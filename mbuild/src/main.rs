use clap::{Args, Parser, Subcommand, ValueEnum};
use nickel_lang_core::error::report::{ColorOpt, ErrorFormat, report};
use nickel_lang_core::serialize::ExportFormat;
use std::env;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use mbuild::store_interpreter::{self, StoreRunOptions};

type MResult<T> = Result<T, MbuildError>;

#[derive(Debug)]
enum MbuildError {
    NickelRecipe {
        files: nickel_lang_core::files::Files,
        error: nickel_lang_core::error::Error,
    },
    InvalidInput(String),
    BuildFailed(String),
}

impl MbuildError {
    fn class(&self) -> &'static str {
        match self {
            Self::NickelRecipe { .. } => "recipe-diagnostic",
            Self::InvalidInput(_) => "invalid-input",
            Self::BuildFailed(_) => "build-failed",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NickelRecipe { .. } => "Nickel recipe error",
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

    recipe_file: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ExportCliFormat {
    Text,
    Json,
    Yaml,
    YamlDocuments,
    Toml,
}

impl From<ExportCliFormat> for ExportFormat {
    fn from(value: ExportCliFormat) -> Self {
        match value {
            ExportCliFormat::Text => ExportFormat::Text,
            ExportCliFormat::Json => ExportFormat::Json,
            ExportCliFormat::Yaml => ExportFormat::Yaml,
            ExportCliFormat::YamlDocuments => ExportFormat::YamlDocuments,
            ExportCliFormat::Toml => ExportFormat::Toml,
        }
    }
}

#[derive(Args, Debug)]
struct ExportCli {
    #[arg(short = 'f', long = "format", default_value = "json")]
    format: ExportCliFormat,

    recipe_file: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(about = "build a Nickel STORE recipe")]
    Build(BuildCli),
    #[command(about = "export a Nickel file with the mbuild STORE environment preloaded")]
    Export(ExportCli),
}

#[derive(Parser, Debug)]
#[command(name = "mbuild")]
#[command(about = "mbuild runtime for Nickel STORE recipes")]
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
        Some(Command::Export(export_cli)) => run_export(export_cli),
        None => build(cli.build),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            match error {
                MbuildError::NickelRecipe { mut files, error } => {
                    report(&mut files, error, ErrorFormat::Text, ColorOpt::Auto);
                }
                other => eprintln!("error[{}]: {}", other.class(), other),
            }
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
        .unwrap_or_else(|| PathBuf::from(".mbuild/recipe.ncl"));
    let rendered = store_interpreter::render_store_recipe_in_workspace_with_options(
        &workspace_root,
        &recipe_path,
        StoreRunOptions {
            emit_progress: !cli.quiet,
        },
        ExportFormat::Text,
    )
    .map_err(map_runtime_error)?;
    print!("{rendered}");
    Ok(())
}

fn run_export(cli: ExportCli) -> MResult<()> {
    let exported = store_interpreter::export_recipe_with_store(&cli.recipe_file, cli.format.into())
        .map_err(map_runtime_error)?;
    print!("{exported}");
    Ok(())
}

fn map_runtime_error(error: mbuild::RuntimeError) -> MbuildError {
    match error {
        mbuild::RuntimeError::RecipeDiagnostic { files, error } => {
            MbuildError::NickelRecipe { files, error }
        }
        mbuild::RuntimeError::InvalidRequest(_)
        | mbuild::RuntimeError::UnknownBuilder(_)
        | mbuild::RuntimeError::RecipeLoad(_) => MbuildError::InvalidInput(error.to_string()),
        mbuild::RuntimeError::Build(_) | mbuild::RuntimeError::Store(_) => {
            MbuildError::BuildFailed(error.to_string())
        }
    }
}
