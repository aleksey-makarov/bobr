use clap::{Parser, Subcommand};
use mbuild_core::Builder;
use nickel_lang::{Context as NickelContext, ErrorFormat as NickelErrorFormat};
use serde::Deserialize;
use serde_json::{Map, Value};
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
    InvalidCommand(String),
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
            Self::InvalidCommand(_) => "invalid-command",
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
            | Self::InvalidCommand(message)
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
#[command(
    after_help = "Examples:\n  mbuild zstd-1.5.7\n  mbuild zstd-1.5.7 build\n  mbuild build zstd-1.5.7\n  mbuild info zstd-1.5.7\n  mbuild verbs github\n  mbuild verbs zstd-1.5.7"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Parse recipes and load one artifact recipe.
    Build {
        /// Artifact name (case-sensitive key in .mbuild/recipes.ncl).
        artifact_name: String,
    },
    /// Show supported verbs for a recipe type (or artifact name).
    Verbs {
        /// Recipe type (for example: github, binary) or artifact name.
        target: String,
    },
    /// Show recipe information for one artifact.
    Info {
        /// Artifact name (case-sensitive key in .mbuild/recipes.ncl).
        artifact_name: String,
    },
    #[command(external_subcommand)]
    External(Vec<String>),
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

const BUILD_VERBS: &[&str] = &["build"];

struct GithubBuilder;
struct BinaryBuilder;

impl Builder for GithubBuilder {
    fn get_type(&self) -> &'static str {
        "github"
    }

    fn verbs(&self) -> &'static [&'static str] {
        BUILD_VERBS
    }
}

impl Builder for BinaryBuilder {
    fn get_type(&self) -> &'static str {
        "binary"
    }

    fn verbs(&self) -> &'static [&'static str] {
        BUILD_VERBS
    }
}

static GITHUB_BUILDER: GithubBuilder = GithubBuilder;
static BINARY_BUILDER: BinaryBuilder = BinaryBuilder;

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
            "unknown verb '{}' for artifact '{}'; supported verbs: {}",
            verb,
            artifact_name,
            supported_verbs_for_artifact(artifact_name)?.join(", ")
        ))),
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

fn run_verbs(target: &str) -> MResult<()> {
    if let Some(verbs) = supported_verbs_for_type(target) {
        println!("type: {target}");
        println!("verbs: {}", verbs.join(", "));
        return Ok(());
    }

    let recipe_type = artifact_recipe_type(target)?;
    let verbs = supported_verbs_for_type(&recipe_type).ok_or_else(|| {
        MbuildError::InvalidCommand(format!(
            "no registered verbs for type '{}'",
            recipe_type
        ))
    })?;
    println!("artifact: {target}");
    println!("type: {recipe_type}");
    println!("verbs: {}", verbs.join(", "));
    Ok(())
}

fn run_info(artifact_name: &str) -> MResult<()> {
    let layout = workspace_layout()?;
    let recipes = load_recipes_object(&layout.recipes)?;
    let recipe_value = recipes.get(artifact_name).ok_or_else(|| {
        MbuildError::ArtifactNotFound(format!(
            "artifact '{}' was not found in recipes '{}'",
            artifact_name,
            layout.recipes.display()
        ))
    })?;
    let recipe_type = recipe_type_from_value(recipe_value)?;
    let (inputs, outputs) = io_counts(recipe_value);

    println!("info: ok");
    println!("recipes: {}", layout.recipes.display());
    println!("artifact: {artifact_name}");
    println!("type: {recipe_type}");
    println!("inputs: {inputs}");
    println!("outputs: {outputs}");
    if let Some(verbs) = supported_verbs_for_type(&recipe_type) {
        println!("verbs: {}", verbs.join(", "));
    } else {
        println!("verbs: (none: unknown type)");
    }

    Ok(())
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
    let object = load_recipes_object(recipes_path)?;
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

fn load_recipes_object(recipes_path: &Path) -> MResult<Map<String, Value>> {
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
    Ok(object.clone())
}

fn artifact_recipe_type(artifact_name: &str) -> MResult<String> {
    let layout = workspace_layout()?;
    let recipes = load_recipes_object(&layout.recipes)?;
    let recipe_value = recipes.get(artifact_name).ok_or_else(|| {
        MbuildError::ArtifactNotFound(format!(
            "artifact '{}' was not found in recipes '{}'",
            artifact_name,
            layout.recipes.display()
        ))
    })?;
    recipe_type_from_value(recipe_value)
}

fn recipe_type_from_value(value: &Value) -> MResult<String> {
    let object = value.as_object().ok_or_else(|| {
        MbuildError::InvalidRecipe("recipe must be an object".to_string())
    })?;
    let type_value = object.get("type").ok_or_else(|| {
        MbuildError::InvalidRecipe("recipe must define 'type'".to_string())
    })?;
    type_value
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| MbuildError::InvalidRecipe("field 'type' must be a string".to_string()))
}

fn io_counts(recipe_value: &Value) -> (usize, usize) {
    let Some(obj) = recipe_value.as_object() else {
        return (0, 0);
    };
    let inputs = obj
        .get("inputs")
        .and_then(Value::as_array)
        .map_or(0, |xs| xs.len());
    let outputs = obj
        .get("outputs")
        .and_then(Value::as_array)
        .map_or(0, |xs| xs.len());
    (inputs, outputs)
}

fn supported_verbs_for_type(recipe_type: &str) -> Option<Vec<&'static str>> {
    registered_builders()
        .iter()
        .find(|builder| builder.get_type() == recipe_type)
        .map(|builder| builder.verbs().to_vec())
}

fn supported_verbs_for_artifact(artifact_name: &str) -> MResult<Vec<&'static str>> {
    let recipe_type = artifact_recipe_type(artifact_name)?;
    Ok(supported_verbs_for_type(&recipe_type).unwrap_or_else(|| vec!["build"]))
}

fn registered_builders() -> [&'static dyn Builder; 2] {
    [&GITHUB_BUILDER, &BINARY_BUILDER]
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
