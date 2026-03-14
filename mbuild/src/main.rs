use clap::{Parser, Subcommand};
use std::fmt;
use std::process::ExitCode;

mod builders;

type MResult<T> = Result<T, MbuildError>;

#[derive(Debug)]
enum MbuildError {
    InvalidCommand(String),
    NotImplemented(String),
}

impl MbuildError {
    fn class(&self) -> &'static str {
        match self {
            Self::InvalidCommand(_) => "invalid-command",
            Self::NotImplemented(_) => "not-implemented",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidCommand(message) | Self::NotImplemented(message) => message,
        }
    }
}

impl fmt::Display for MbuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

#[derive(Parser, Debug)]
#[command(name = "mbuild")]
#[command(about = "Unified mbuild CLI (term runtime pending)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Build { artifact_name: String },
    Verbs { target: String },
    Info { artifact_name: String },
    #[command(external_subcommand)]
    External(Vec<String>),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class(), error);
            ExitCode::from(1)
        }
    }
}

fn run(command: Command) -> MResult<()> {
    match command {
        Command::Build { artifact_name } => run_build(&artifact_name),
        Command::Verbs { target } => run_verbs(&target),
        Command::Info { artifact_name } => run_info(&artifact_name),
        Command::External(args) => run_artifact_form(&args),
    }
}

fn run_artifact_form(args: &[String]) -> MResult<()> {
    if args.is_empty() {
        return Err(MbuildError::InvalidCommand(
            "expected '<artifact> [verb]'".to_string(),
        ));
    }
    if args.len() > 2 {
        return Err(MbuildError::InvalidCommand(format!(
            "too many positional arguments; expected '<artifact> [verb]', got: '{}'",
            args.join(" ")
        )));
    }

    let artifact_name = &args[0];
    let verb = args.get(1).map(String::as_str).unwrap_or("build");
    match verb {
        "build" => run_build(artifact_name),
        _ => Err(MbuildError::InvalidCommand(format!(
            "verb '{}' is not supported by the current transitional CLI",
            verb
        ))),
    }
}

fn run_build(artifact_name: &str) -> MResult<()> {
    Err(MbuildError::NotImplemented(format!(
        "term-based runtime is not implemented yet; build '{}' cannot be executed through the CLI in this step",
        artifact_name
    )))
}

fn run_info(artifact_name: &str) -> MResult<()> {
    Err(MbuildError::NotImplemented(format!(
        "term-based runtime is not implemented yet; info '{}' is unavailable in this step",
        artifact_name
    )))
}

fn run_verbs(target: &str) -> MResult<()> {
    if let Some(builder) = builders::get_builder(target) {
        println!("builder: {}", builder.spec().tag);
        println!("verbs: build");
        return Ok(());
    }

    let tags = builders::supported_builder_tags();
    Err(MbuildError::InvalidCommand(format!(
        "unknown builder '{}'; supported builders: {}",
        target,
        tags.join(", ")
    )))
}
