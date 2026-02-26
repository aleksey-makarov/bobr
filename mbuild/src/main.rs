use clap::{Parser, Subcommand};
use nickel_lang::{Context as NickelContext, ErrorFormat as NickelErrorFormat};
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const ROOT_DIR: &str = ".mbuild";
const RECIPES_FILE: &str = "recipes.ncl";

type MResult<T> = Result<T, MbuildError>;

#[derive(Debug)]
enum MbuildError {
    ConfigNotFound(String),
    ConfigEvalFailed(String),
    ArtifactNotFound(String),
    InvalidRecipe(String),
    NotImplemented(String),
    FsFailed(String),
}

impl MbuildError {
    fn class(&self) -> &'static str {
        match self {
            Self::ConfigNotFound(_) => "config-not-found",
            Self::ConfigEvalFailed(_) => "config-eval-failed",
            Self::ArtifactNotFound(_) => "artifact-not-found",
            Self::InvalidRecipe(_) => "invalid-recipe",
            Self::NotImplemented(_) => "not-implemented",
            Self::FsFailed(_) => "fs-failed",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::ConfigNotFound(message)
            | Self::ConfigEvalFailed(message)
            | Self::ArtifactNotFound(message)
            | Self::InvalidRecipe(message)
            | Self::NotImplemented(message)
            | Self::FsFailed(message) => message,
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
#[command(about = "Unified mbuild CLI (skeleton)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Parse recipes.ncl and load one artifact recipe.
    Build {
        /// Artifact name (case-sensitive key in .mbuild/recipes.ncl).
        artifact_name: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum UnifiedRecipe {
    #[serde(rename = "github")]
    Github {
        #[serde(default)]
        inputs: Vec<String>,
        #[serde(default)]
        outputs: Vec<String>,
        repo: String,
        commit: String,
    },
    #[serde(rename = "binary")]
    Binary {
        #[serde(default)]
        inputs: Vec<String>,
        #[serde(default)]
        outputs: Vec<String>,
        script: String,
    },
}

impl UnifiedRecipe {
    fn recipe_type(&self) -> &'static str {
        match self {
            Self::Github { .. } => "github",
            Self::Binary { .. } => "binary",
        }
    }

    fn inputs(&self) -> &[String] {
        match self {
            Self::Github { inputs, .. } | Self::Binary { inputs, .. } => inputs,
        }
    }

    fn outputs(&self) -> &[String] {
        match self {
            Self::Github { outputs, .. } | Self::Binary { outputs, .. } => outputs,
        }
    }
}

struct WorkspaceLayout {
    recipes: PathBuf,
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
    }
}

fn run_build(artifact_name: &str) -> MResult<()> {
    let layout = workspace_layout()?;
    let recipe = load_unified_recipe(&layout.recipes, artifact_name)?;
    validate_common_fields(&recipe)?;

    println!("build: parsed");
    println!("recipes: {}", layout.recipes.display());
    println!("artifact: {artifact_name}");
    println!("type: {}", recipe.recipe_type());
    println!("inputs: {}", recipe.inputs().len());
    println!("outputs: {}", recipe.outputs().len());
    match &recipe {
        UnifiedRecipe::Github { repo, commit, .. } => {
            println!("repo: {repo}");
            println!("commit: {commit}");
        }
        UnifiedRecipe::Binary { script, .. } => {
            println!("script_bytes: {}", script.len());
        }
    }

    Err(MbuildError::NotImplemented(
        "dispatch to concrete builders is not implemented yet".to_string(),
    ))
}

fn workspace_layout() -> MResult<WorkspaceLayout> {
    let cwd = env::current_dir().map_err(|error| {
        MbuildError::FsFailed(format!("failed to get current directory: {error}"))
    })?;
    let root = cwd.join(ROOT_DIR);
    Ok(WorkspaceLayout {
        recipes: root.join(RECIPES_FILE),
    })
}

fn load_unified_recipe(recipes_path: &Path, artifact_name: &str) -> MResult<UnifiedRecipe> {
    if !recipes_path.exists() {
        return Err(MbuildError::ConfigNotFound(format!(
            "default recipes file '{}' was not found",
            recipes_path.display()
        )));
    }

    let json_value = eval_nickel_file_to_json(recipes_path)?;
    let object = json_value.as_object().ok_or_else(|| {
        MbuildError::InvalidRecipe("Nickel recipes must export an object at top level".to_string())
    })?;

    let artifact_value = object.get(artifact_name).ok_or_else(|| {
        MbuildError::ArtifactNotFound(format!(
            "artifact '{}' was not found in recipes '{}'",
            artifact_name,
            recipes_path.display()
        ))
    })?;

    serde_json::from_value::<UnifiedRecipe>(artifact_value.clone()).map_err(|error| {
        MbuildError::InvalidRecipe(format!(
            "invalid unified recipe for artifact '{}': {error}",
            artifact_name
        ))
    })
}

fn validate_common_fields(recipe: &UnifiedRecipe) -> MResult<()> {
    for name in recipe.inputs() {
        validate_name(name)?;
    }
    for name in recipe.outputs() {
        validate_name(name)?;
    }
    Ok(())
}

fn validate_name(name: &str) -> MResult<()> {
    if name.is_empty() {
        return Err(MbuildError::InvalidRecipe(
            "input/output name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(MbuildError::InvalidRecipe(format!(
            "invalid input/output name '{}'",
            name
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(MbuildError::InvalidRecipe(format!(
            "invalid input/output name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

fn eval_nickel_file_to_json(path: &Path) -> MResult<Value> {
    let source = fs::read_to_string(path).map_err(|error| {
        MbuildError::ConfigEvalFailed(format!("failed to read Nickel file '{}': {error}", path.display()))
    })?;

    let import_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .as_os_str()
        .to_os_string();

    let mut context = NickelContext::new()
        .with_source_name(path.display().to_string())
        .with_added_import_paths(vec![import_dir]);

    let expr = context
        .eval_deep_for_export(&source)
        .map_err(|error| MbuildError::ConfigEvalFailed(format_nickel_error(error)))?;

    expr.to_serde()
        .map_err(|error| MbuildError::ConfigEvalFailed(format!("failed to deserialize evaluated Nickel value: {error}")))
}

fn format_nickel_error(error: nickel_lang::Error) -> String {
    let mut out = Vec::<u8>::new();
    match error.format(&mut out, NickelErrorFormat::Text) {
        Ok(()) => {
            let rendered = String::from_utf8_lossy(&out).trim().to_string();
            if rendered.is_empty() {
                "Nickel evaluation failed with empty diagnostics".to_string()
            } else {
                rendered
            }
        }
        Err(format_error) => format!(
            "Nickel evaluation failed; could not render diagnostics: {format_error}; original: {error:?}"
        ),
    }
}
