use clap::{Parser, Subcommand};
use nickel_lang::{Context as NickelContext, ErrorFormat as NickelErrorFormat};
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const ROOT_DIR: &str = ".mbuild";
const RECIPES_FILE: &str = "recipes.ncl";
const MATERIALIZED_DIR: &str = "materialized";
const STANDARD_IMAGE: &str =
    "docker.io/library/gcc@sha256:99732c3fbda294e6e7c8bb463a98ec394d48de16ee45fece6f28d7bf7d9dbd99";

type MResult<T> = Result<T, MbbinError>;

#[derive(Debug)]
enum MbbinError {
    ConfigNotFound(String),
    ConfigEvalFailed(String),
    ArtifactNotFound(String),
    InvalidRecipe(String),
    PodmanFailed(String),
    BuildFailed(String),
    FsFailed(String),
}

impl MbbinError {
    fn class(&self) -> &'static str {
        match self {
            Self::ConfigNotFound(_) => "config-not-found",
            Self::ConfigEvalFailed(_) => "config-eval-failed",
            Self::ArtifactNotFound(_) => "artifact-not-found",
            Self::InvalidRecipe(_) => "invalid-recipe",
            Self::PodmanFailed(_) => "podman-failed",
            Self::BuildFailed(_) => "build-failed",
            Self::FsFailed(_) => "fs-failed",
        }
    }
    fn message(&self) -> &str {
        match self {
            Self::ConfigNotFound(message)
            | Self::ConfigEvalFailed(message)
            | Self::ArtifactNotFound(message)
            | Self::InvalidRecipe(message)
            | Self::PodmanFailed(message)
            | Self::BuildFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "mbbin")]
#[command(about = "Minimal binary builder (MVP)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Verify that podman CLI is available.
    Doctor,
    /// Build one artifact from .mbuild/recipes.ncl.
    Build {
        /// Artifact name (case-sensitive key in .mbuild/recipes.ncl).
        artifact_name: String,
    },
}

#[derive(Debug, Deserialize)]
struct BinRecipe {
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
    script: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class(), error.message());
            ExitCode::from(1)
        }
    }
}

fn run(command: Command) -> MResult<()> {
    match command {
        Command::Doctor => run_doctor(),
        Command::Build { artifact_name } => run_build(&artifact_name),
    }
}

fn run_doctor() -> MResult<()> {
    let output = ProcessCommand::new("podman")
        .arg("--version")
        .output()
        .map_err(|error| MbbinError::PodmanFailed(format!("failed to execute podman: {error}")))?;

    if !output.status.success() {
        return Err(MbbinError::PodmanFailed(format!(
            "podman --version failed: {}",
            command_details(&output)
        )));
    }

    println!("doctor: ok");
    println!("{}", String::from_utf8_lossy(&output.stdout).trim());
    Ok(())
}

fn run_build(artifact_name: &str) -> MResult<()> {
    let layout = workspace_layout()?;
    ensure_base_dirs(&layout)?;

    let recipe = load_recipe_from_recipes(&layout.recipes, artifact_name)?;
    validate_recipe(&recipe)?;

    for input in &recipe.inputs {
        let input_dir = layout.materialized.join(input);
        if !input_dir.is_dir() {
            return Err(MbbinError::BuildFailed(format!(
                "input '{}' does not exist as a directory: {}",
                input,
                input_dir.display()
            )));
        }
    }

    for output_name in &recipe.outputs {
        recreate_empty_dir(&layout.materialized.join(output_name))?;
    }

    let script_path = write_temp_script(artifact_name, &recipe.script)?;
    let build_result = run_container_build(&layout, &recipe, &script_path);
    let _ = fs::remove_file(&script_path);
    build_result?;

    println!("build: ok");
    println!("recipes: {}", layout.recipes.display());
    println!("artifact: {artifact_name}");
    println!("image: {STANDARD_IMAGE}");
    Ok(())
}

fn run_container_build(
    layout: &WorkspaceLayout,
    recipe: &BinRecipe,
    script_path: &Path,
) -> MResult<()> {
    let (uid, gid) = current_uid_gid();
    let script_mount = format!("{}:/__mbbin_script:ro", script_path.display());

    let mut process = ProcessCommand::new("podman");
    process
        .arg("run")
        .arg("--rm")
        .arg("--network=none")
        .arg("--userns=keep-id")
        .arg("--user")
        .arg(format!("{uid}:{gid}"));

    for input in &recipe.inputs {
        let host_path = layout.materialized.join(input);
        process
            .arg("--volume")
            .arg(format!("{}:/in/{}:O", host_path.display(), input));
    }

    for output in &recipe.outputs {
        let host_path = layout.materialized.join(output);
        process
            .arg("--volume")
            .arg(format!("{}:/out/{}:rw", host_path.display(), output));
    }

    process
        .arg("--volume")
        .arg(script_mount)
        .arg(STANDARD_IMAGE)
        .arg("/__mbbin_script");

    let output = process.output().map_err(|error| {
        MbbinError::PodmanFailed(format!("failed to execute podman run: {error}"))
    })?;

    if !output.status.success() {
        return Err(MbbinError::BuildFailed(format!(
            "podman run failed with exit status {}: {}",
            output.status.code().unwrap_or(1),
            command_details(&output)
        )));
    }

    if !output.stdout.is_empty() {
        println!("{}", String::from_utf8_lossy(&output.stdout).trim_end());
    }
    Ok(())
}

fn command_details(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        "command failed without output".to_string()
    }
}

fn load_recipe_from_recipes(recipes_path: &Path, artifact_name: &str) -> MResult<BinRecipe> {
    if !recipes_path.exists() {
        return Err(MbbinError::ConfigNotFound(format!(
            "default recipes file '{}' was not found",
            recipes_path.display()
        )));
    }

    let json_value = eval_nickel_file_to_json(recipes_path)?;
    let object = json_value.as_object().ok_or_else(|| {
        MbbinError::InvalidRecipe("Nickel recipes must export an object at top level".to_string())
    })?;

    let artifact_value = object.get(artifact_name).ok_or_else(|| {
        MbbinError::ArtifactNotFound(format!(
            "artifact '{}' was not found in recipes '{}'",
            artifact_name,
            recipes_path.display()
        ))
    })?;

    serde_json::from_value::<BinRecipe>(artifact_value.clone()).map_err(|error| {
        MbbinError::InvalidRecipe(format!(
            "invalid recipe for artifact '{}': {error}",
            artifact_name
        ))
    })
}

fn validate_recipe(recipe: &BinRecipe) -> MResult<()> {
    for name in &recipe.inputs {
        validate_mount_name(name)?;
    }
    for name in &recipe.outputs {
        validate_mount_name(name)?;
    }
    if !recipe.script.starts_with("#!") {
        return Err(MbbinError::InvalidRecipe(
            "script must start with shebang (`#!`)".to_string(),
        ));
    }
    Ok(())
}

fn validate_mount_name(name: &str) -> MResult<()> {
    if name.is_empty() {
        return Err(MbbinError::InvalidRecipe(
            "input/output name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(MbbinError::InvalidRecipe(format!(
            "invalid input/output name '{name}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(MbbinError::InvalidRecipe(format!(
            "invalid input/output name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

fn eval_nickel_file_to_json(path: &Path) -> MResult<Value> {
    let source = fs::read_to_string(path).map_err(|error| {
        MbbinError::ConfigEvalFailed(format!(
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
        .map_err(|error| MbbinError::ConfigEvalFailed(format_nickel_error(error)))?;

    expr.to_serde().map_err(|error| {
        MbbinError::ConfigEvalFailed(format!(
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

fn write_temp_script(artifact_name: &str, script: &str) -> MResult<PathBuf> {
    let tmp_dir = env::temp_dir();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| MbbinError::FsFailed(format!("system time before UNIX_EPOCH: {error}")))?
        .as_nanos();
    let path = tmp_dir.join(format!("mbbin-{artifact_name}-{now}.script"));

    fs::write(&path, script).map_err(|error| {
        MbbinError::FsFailed(format!(
            "failed to write temporary script '{}': {error}",
            path.display()
        ))
    })?;

    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&path, perms).map_err(|error| {
            MbbinError::FsFailed(format!(
                "failed to set executable permissions on '{}': {error}",
                path.display()
            ))
        })?;
    }

    Ok(path)
}

fn recreate_empty_dir(path: &Path) -> MResult<()> {
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path).map_err(|error| {
                MbbinError::FsFailed(format!(
                    "failed to remove previous output directory '{}': {error}",
                    path.display()
                ))
            })?;
        } else {
            fs::remove_file(path).map_err(|error| {
                MbbinError::FsFailed(format!(
                    "failed to remove previous output file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        MbbinError::FsFailed(format!(
            "failed to create output directory '{}': {error}",
            path.display()
        ))
    })
}

fn current_uid_gid() -> (u32, u32) {
    #[cfg(unix)]
    {
        // Safe: libc returns process credentials and has no side effects.
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        (uid, gid)
    }

    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

struct WorkspaceLayout {
    root: PathBuf,
    recipes: PathBuf,
    materialized: PathBuf,
}

fn workspace_layout() -> MResult<WorkspaceLayout> {
    let cwd = env::current_dir().map_err(|error| {
        MbbinError::FsFailed(format!("failed to get current directory: {error}"))
    })?;
    let root = cwd.join(ROOT_DIR);
    Ok(WorkspaceLayout {
        recipes: root.join(RECIPES_FILE),
        materialized: root.join(MATERIALIZED_DIR),
        root,
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> MResult<()> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.materialized, "materialized")?;
    Ok(())
}

fn ensure_dir(path: &Path, label: &str) -> MResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        MbbinError::FsFailed(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}
