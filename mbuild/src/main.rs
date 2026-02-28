use clap::{Parser, Subcommand};
use mbuild_core::BuilderError;
use nickel_lang::{Context as NickelContext, ErrorFormat as NickelErrorFormat};
use serde_json::{Map, Value};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod builders;

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
    BuilderFailed(String),
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
            Self::BuilderFailed(_) => "builder-failed",
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
            | Self::BuilderFailed(message)
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
    after_help = "Examples:\n  mbuild zstd-1.5.7\n  mbuild zstd-1.5.7 build\n  mbuild zstd-1.5.7 cache\n  mbuild build zstd-1.5.7\n  mbuild info zstd-1.5.7\n  mbuild verbs github\n  mbuild verbs zstd-1.5.7"
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
        _ => run_custom_verb(artifact_name, verb),
    }
}

struct ArtifactContext {
    layout: WorkspaceLayout,
    artifact_value: Value,
    recipe_type: String,
}

fn load_artifact_context(artifact_name: &str) -> MResult<ArtifactContext> {
    let layout = workspace_layout()?;
    let artifact_value = load_recipe_value(&layout.recipes, artifact_name)?;
    validate_common_fields(&artifact_value)?;
    let recipe_type = recipe_type_from_value(&artifact_value)?;
    Ok(ArtifactContext {
        layout,
        artifact_value,
        recipe_type,
    })
}

fn run_build(artifact_name: &str) -> MResult<()> {
    let ctx = load_artifact_context(artifact_name)?;
    let builder = builders::get_builder(&ctx.recipe_type).ok_or_else(|| {
        MbuildError::InvalidCommand(format!(
            "no builder is registered for type '{}'",
            ctx.recipe_type
        ))
    })?;

    println!("build: parsed");
    println!("recipes: {}", ctx.layout.recipes.display());
    println!("artifact: {artifact_name}");
    println!("type: {}", ctx.recipe_type);
    let (inputs, outputs) = io_counts(&ctx.artifact_value);
    println!("inputs: {inputs}");
    println!("outputs: {outputs}");

    for (key, value) in builder
        .summarize_recipe(&ctx.artifact_value)
        .map_err(map_builder_error)?
    {
        println!("{key}: {value}");
    }

    builder
        .run_build(artifact_name, &ctx.artifact_value)
        .map_err(map_builder_error)
}

fn run_custom_verb(artifact_name: &str, verb: &str) -> MResult<()> {
    let ctx = load_artifact_context(artifact_name)?;
    let builder = builders::get_builder(&ctx.recipe_type).ok_or_else(|| {
        MbuildError::InvalidCommand(format!(
            "no builder is registered for type '{}'",
            ctx.recipe_type
        ))
    })?;
    builder
        .run_custom(verb, artifact_name, &ctx.artifact_value)
        .map_err(map_builder_error)
}

fn run_verbs(target: &str) -> MResult<()> {
    if let Some(verbs) = supported_verbs_for_type(target) {
        println!("type: {target}");
        println!("verbs: {}", verbs.join(", "));
        return Ok(());
    }

    let recipe_type = artifact_recipe_type(target)?;
    let verbs = supported_verbs_for_type(&recipe_type).ok_or_else(|| {
        MbuildError::InvalidCommand(format!("no registered verbs for type '{}'", recipe_type))
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

fn load_recipe_value(recipes_path: &Path, artifact_name: &str) -> MResult<Value> {
    let object = load_recipes_object(recipes_path)?;
    let artifact_value = object.get(artifact_name).ok_or_else(|| {
        MbuildError::ArtifactNotFound(format!(
            "artifact '{}' was not found in recipes '{}'",
            artifact_name,
            recipes_path.display()
        ))
    })?;
    Ok(artifact_value.clone())
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
    let object = value
        .as_object()
        .ok_or_else(|| MbuildError::InvalidRecipe("recipe must be an object".to_string()))?;
    let type_value = object
        .get("type")
        .ok_or_else(|| MbuildError::InvalidRecipe("recipe must define 'type'".to_string()))?;
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
    builders::supported_verbs_for_type(recipe_type)
}

fn validate_common_fields(recipe: &Value) -> MResult<()> {
    let Some(obj) = recipe.as_object() else {
        return Err(MbuildError::InvalidRecipe(
            "recipe must be an object".to_string(),
        ));
    };
    if let Some(inputs) = obj.get("inputs").and_then(Value::as_array) {
        for input in inputs {
            let name = input.as_str().ok_or_else(|| {
                MbuildError::InvalidRecipe("each input name must be a string".to_string())
            })?;
            validate_name(name)?;
        }
    }
    if let Some(outputs) = obj.get("outputs").and_then(Value::as_array) {
        for output in outputs {
            let name = output.as_str().ok_or_else(|| {
                MbuildError::InvalidRecipe("each output name must be a string".to_string())
            })?;
            validate_name(name)?;
        }
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
        MbuildError::ConfigEvalFailed(format!(
            "failed to read Nickel file '{}': {error}",
            path.display()
        ))
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

    expr.to_serde().map_err(|error| {
        MbuildError::ConfigEvalFailed(format!(
            "failed to deserialize evaluated Nickel value: {error}"
        ))
    })
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

fn map_builder_error(error: BuilderError) -> MbuildError {
    match error {
        BuilderError::NotImplemented(message) => MbuildError::NotImplemented(message),
        BuilderError::UnsupportedVerb(message) => MbuildError::InvalidCommand(message),
        BuilderError::InvalidRecipe(message) => MbuildError::InvalidRecipe(message),
        BuilderError::ExecutionFailed(message) => MbuildError::BuilderFailed(message),
    }
}
