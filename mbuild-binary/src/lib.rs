use mbuild_core::{Builder, BuilderError};
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

const ROOT_DIR: &str = ".mbuild";
const MATERIALIZED_DIR: &str = "materialized";
const STANDARD_IMAGE: &str =
    "docker.io/library/gcc@sha256:99732c3fbda294e6e7c8bb463a98ec394d48de16ee45fece6f28d7bf7d9dbd99";

type BResult<T> = Result<T, BinaryError>;

#[derive(Debug)]
enum BinaryError {
    InvalidRecipe(String),
    PodmanFailed(String),
    BuildFailed(String),
    FsFailed(String),
}

impl BinaryError {
}

#[derive(Debug, Deserialize)]
struct BinaryRecipe {
    #[serde(rename = "type")]
    recipe_type: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
    script: String,
}

pub struct BinaryBuilder;

impl Builder for BinaryBuilder {
    fn get_type(&self) -> &'static str {
        "binary"
    }

    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        let recipe = parse_recipe(recipe)?;
        let layout = workspace_layout().map_err(map_error)?;
        ensure_base_dirs(&layout).map_err(map_error)?;

        for input in &recipe.inputs {
            let input_dir = layout.materialized.join(input);
            if !input_dir.is_dir() {
                return Err(BuilderError::ExecutionFailed(format!(
                    "input '{}' does not exist as a directory: {}",
                    input,
                    input_dir.display()
                )));
            }
        }

        for output_name in &recipe.outputs {
            recreate_empty_dir(&layout.materialized.join(output_name)).map_err(map_error)?;
        }

        let script_path = write_temp_script(artifact, &recipe.script).map_err(map_error)?;
        let build_result = run_container_build(&layout, &recipe, &script_path);
        let _ = fs::remove_file(&script_path);
        build_result.map_err(map_error)?;

        println!("build: ok");
        println!("artifact: {artifact}");
        println!("image: {STANDARD_IMAGE}");
        Ok(())
    }

    fn summarize_recipe(&self, recipe: &Value) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let recipe = parse_recipe(recipe)?;
        Ok(vec![("script_bytes", recipe.script.len().to_string())])
    }
}

fn parse_recipe(value: &Value) -> Result<BinaryRecipe, BuilderError> {
    serde_json::from_value::<BinaryRecipe>(value.clone())
        .map_err(|error| BuilderError::InvalidRecipe(format!("invalid binary recipe: {error}")))
        .and_then(|recipe| {
            validate_recipe(&recipe).map_err(map_error)?;
            Ok(recipe)
        })
}

fn validate_recipe(recipe: &BinaryRecipe) -> BResult<()> {
    if recipe.recipe_type != "binary" {
        return Err(BinaryError::InvalidRecipe(
            "type must be 'binary'".to_string(),
        ));
    }

    for name in &recipe.inputs {
        validate_mount_name(name)?;
    }
    for name in &recipe.outputs {
        validate_mount_name(name)?;
    }
    if !recipe.script.starts_with("#!") {
        return Err(BinaryError::InvalidRecipe(
            "script must start with shebang (`#!`)".to_string(),
        ));
    }
    Ok(())
}

fn validate_mount_name(name: &str) -> BResult<()> {
    if name.is_empty() {
        return Err(BinaryError::InvalidRecipe(
            "input/output name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(BinaryError::InvalidRecipe(format!(
            "invalid input/output name '{name}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(BinaryError::InvalidRecipe(format!(
            "invalid input/output name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

fn run_container_build(
    layout: &WorkspaceLayout,
    recipe: &BinaryRecipe,
    script_path: &Path,
) -> BResult<()> {
    let (uid, gid) = current_uid_gid();
    let script_mount = format!("{}:/__mbuild_binary_script:ro", script_path.display());

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
        .arg("/__mbuild_binary_script");

    let output = process.output().map_err(|error| {
        BinaryError::PodmanFailed(format!("failed to execute podman run: {error}"))
    })?;

    if !output.status.success() {
        return Err(BinaryError::BuildFailed(format!(
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

fn write_temp_script(artifact_name: &str, script: &str) -> BResult<PathBuf> {
    let tmp_dir = env::temp_dir();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BinaryError::FsFailed(format!("system time before UNIX_EPOCH: {error}")))?
        .as_nanos();
    let path = tmp_dir.join(format!("mbuild-binary-{artifact_name}-{now}.script"));

    fs::write(&path, script).map_err(|error| {
        BinaryError::FsFailed(format!(
            "failed to write temporary script '{}': {error}",
            path.display()
        ))
    })?;

    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&path, perms).map_err(|error| {
            BinaryError::FsFailed(format!(
                "failed to set executable permissions on '{}': {error}",
                path.display()
            ))
        })?;
    }

    Ok(path)
}

fn recreate_empty_dir(path: &Path) -> BResult<()> {
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path).map_err(|error| {
                BinaryError::FsFailed(format!(
                    "failed to remove previous output directory '{}': {error}",
                    path.display()
                ))
            })?;
        } else {
            fs::remove_file(path).map_err(|error| {
                BinaryError::FsFailed(format!(
                    "failed to remove previous output file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        BinaryError::FsFailed(format!(
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
    materialized: PathBuf,
}

fn workspace_layout() -> BResult<WorkspaceLayout> {
    let cwd = env::current_dir().map_err(|error| {
        BinaryError::FsFailed(format!("failed to get current directory: {error}"))
    })?;
    let root = cwd.join(ROOT_DIR);
    Ok(WorkspaceLayout {
        materialized: root.join(MATERIALIZED_DIR),
        root,
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> BResult<()> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.materialized, "materialized")?;
    Ok(())
}

fn ensure_dir(path: &Path, label: &str) -> BResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        BinaryError::FsFailed(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

fn map_error(error: BinaryError) -> BuilderError {
    match error {
        BinaryError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        BinaryError::PodmanFailed(message)
        | BinaryError::BuildFailed(message)
        | BinaryError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}
