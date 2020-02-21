use clap::{Parser, Subcommand};
use nickel_lang::{Context as NickelContext, ErrorFormat as NickelErrorFormat};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const ROOT_DIR: &str = ".mbuild";
const RECIPES_FILE: &str = "recipes.ncl";
const SHARED_STATE_FILE: &str = "state.ncl";
const INTERNAL_STATE_FILE: &str = "internal.ncl";
const BUILDER_DIR: &str = "github";
const MIRRORS_DIR: &str = "mirrors";
const MATERIALIZED_DIR: &str = "materialized";

type MResult<T> = Result<T, MbsrcError>;

#[derive(Debug)]
enum ErrorClass {
    ConfigNotFound,
    ConfigEvalFailed,
    ArtifactNotFound,
    InvalidRecipe,
    GitFailed,
    CommitNotFound,
    StateFailed,
    MaterializeFailed,
    DematerializeFailed,
    FsFailed,
}

impl ErrorClass {
    fn as_str(&self) -> &'static str {
        match self {
            Self::ConfigNotFound => "config-not-found",
            Self::ConfigEvalFailed => "config-eval-failed",
            Self::ArtifactNotFound => "artifact-not-found",
            Self::InvalidRecipe => "invalid-recipe",
            Self::GitFailed => "git-failed",
            Self::CommitNotFound => "commit-not-found",
            Self::StateFailed => "state-failed",
            Self::MaterializeFailed => "materialize-failed",
            Self::DematerializeFailed => "dematerialize-failed",
            Self::FsFailed => "fs-failed",
        }
    }
}

#[derive(Debug)]
struct MbsrcError {
    class: ErrorClass,
    message: String,
}

impl MbsrcError {
    fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "mbsrc")]
#[command(about = "Minimal source builder (MVP)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build one artifact from .mbuild/recipes.ncl.
    Build {
        /// Artifact name (case-sensitive key in .mbuild/recipes.ncl).
        artifact_name: String,
    },
    /// Materialize output by artifact name.
    Materialize {
        /// Artifact name.
        artifact_name: String,
    },
    /// Remove materialized output by artifact name.
    Dematerialize {
        /// Artifact name.
        artifact_name: String,
    },
}

#[derive(Debug, Deserialize)]
struct Recipe {
    source: SourceSpec,
}

#[derive(Debug, Deserialize)]
struct SourceSpec {
    #[serde(rename = "type")]
    source_type: String,
    repo: String,
    commit: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct InternalState {
    builds: Vec<BuildRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BuildRecord {
    artifact: String,
    repo: String,
    commit: String,
    mirror_path: String,
    updated_at_epoch_secs: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SharedState {
    artifacts: Vec<SharedArtifactRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SharedArtifactRecord {
    artifact: String,
    repo: String,
    commit: String,
    #[serde(default)]
    materialized: bool,
    #[serde(default)]
    materialized_path: String,
    #[serde(default)]
    materialized_at_epoch_secs: f64,
    updated_at_epoch_secs: f64,
}

impl Default for InternalState {
    fn default() -> Self {
        Self { builds: Vec::new() }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            artifacts: Vec::new(),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[{}]: {}", error.class.as_str(), error.message);
            ExitCode::from(1)
        }
    }
}

fn run(command: Command) -> MResult<()> {
    let dirs = workspace_layout();
    ensure_base_dirs(&dirs)?;

    match command {
        Command::Build { artifact_name } => run_build(&dirs, &artifact_name),
        Command::Materialize { artifact_name } => run_materialize(&dirs, &artifact_name),
        Command::Dematerialize { artifact_name } => run_dematerialize(&dirs, &artifact_name),
    }
}

fn run_build(dirs: &WorkspaceLayout, artifact_name: &str) -> MResult<()> {
    let recipe = load_recipe_from_config(&dirs.recipes, artifact_name)?;
    validate_recipe(&recipe)?;
    ensure_dir(&dirs.mirrors, "mirrors")?;

    let (owner, repo_name) = parse_github_repo(&recipe.source.repo)?;
    let mirror_name = format!("{owner}_{repo_name}.git");
    let mirror_path = dirs.mirrors.join(mirror_name);

    if mirror_path.exists() {
        if !mirror_path.is_dir() {
            return Err(MbsrcError::new(
                ErrorClass::FsFailed,
                format!(
                    "mirror path exists but is not a directory: {}",
                    mirror_path.display()
                ),
            ));
        }

        let has_commit = git_has_commit(&mirror_path, &recipe.source.commit)?;
        if !has_commit {
            run_git(
                &["fetch", "--all", "--prune"],
                Some(&mirror_path),
                "failed to fetch mirror",
            )?;
        }
    } else {
        run_git(
            &[
                "clone",
                "--mirror",
                &recipe.source.repo,
                &mirror_path.to_string_lossy(),
            ],
            None,
            "failed to clone mirror",
        )?;
    }

    if !git_has_commit(&mirror_path, &recipe.source.commit)? {
        return Err(MbsrcError::new(
            ErrorClass::CommitNotFound,
            format!(
                "commit {} not found in mirror {}",
                recipe.source.commit,
                mirror_path.display()
            ),
        ));
    }

    update_states(
        dirs,
        artifact_name,
        &recipe.source.repo,
        &recipe.source.commit,
        &mirror_path,
    )?;

    println!("build: ok");
    println!("recipes: {}", dirs.recipes.display());
    println!("artifact: {artifact_name}");
    println!("repo: {}", recipe.source.repo);
    println!("commit: {}", recipe.source.commit);
    println!("mirror: {}", mirror_path.display());

    Ok(())
}

fn run_materialize(dirs: &WorkspaceLayout, artifact_name: &str) -> MResult<()> {
    let internal: InternalState = parse_nickel_data_file(&dirs.internal_state)?;
    let build = internal
        .builds
        .iter()
        .find(|record| record.artifact == artifact_name)
        .ok_or_else(|| {
            MbsrcError::new(
                ErrorClass::ArtifactNotFound,
                format!(
                    "artifact '{}' is not built yet; run `mbsrc build {}` first",
                    artifact_name, artifact_name
                ),
            )
        })?;

    let mirror_path = PathBuf::from(&build.mirror_path);
    if !mirror_path.is_dir() {
        return Err(MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "mirror directory does not exist for artifact '{}': {}",
                artifact_name,
                mirror_path.display()
            ),
        ));
    }

    if !git_has_commit(&mirror_path, &build.commit)? {
        return Err(MbsrcError::new(
            ErrorClass::CommitNotFound,
            format!(
                "commit {} not found in mirror {}",
                build.commit,
                mirror_path.display()
            ),
        ));
    }

    let now = current_epoch_secs()?;
    let tmp_base = dirs.root.join(format!(
        ".materialize-{}-{}",
        artifact_name,
        now.trunc() as i64
    ));
    let tmp_dir = tmp_base.with_extension("dir");
    let tmp_tar = tmp_base.with_extension("tar");
    let target_dir = dirs.materialized.join(artifact_name);

    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir).map_err(|error| {
            MbsrcError::new(
                ErrorClass::MaterializeFailed,
                format!(
                    "failed to clean temporary directory '{}': {error}",
                    tmp_dir.display()
                ),
            )
        })?;
    }
    if tmp_tar.exists() {
        fs::remove_file(&tmp_tar).map_err(|error| {
            MbsrcError::new(
                ErrorClass::MaterializeFailed,
                format!(
                    "failed to clean temporary tar '{}': {error}",
                    tmp_tar.display()
                ),
            )
        })?;
    }

    fs::create_dir_all(&tmp_dir).map_err(|error| {
        MbsrcError::new(
            ErrorClass::MaterializeFailed,
            format!(
                "failed to create temporary directory '{}': {error}",
                tmp_dir.display()
            ),
        )
    })?;

    let tmp_tar_string = tmp_tar.to_string_lossy().to_string();
    run_git(
        &[
            "archive",
            "--format=tar",
            "--output",
            &tmp_tar_string,
            &build.commit,
        ],
        Some(&mirror_path),
        "failed to create archive from mirror",
    )?;

    run_command(
        "tar",
        &["-xf", &tmp_tar_string, "-C", &tmp_dir.to_string_lossy()],
        None,
        "failed to extract archive",
    )?;

    if tmp_tar.exists() {
        fs::remove_file(&tmp_tar).map_err(|error| {
            MbsrcError::new(
                ErrorClass::MaterializeFailed,
                format!(
                    "failed to remove temporary tar '{}': {error}",
                    tmp_tar.display()
                ),
            )
        })?;
    }

    if target_dir.exists() {
        fs::remove_dir_all(&target_dir).map_err(|error| {
            MbsrcError::new(
                ErrorClass::MaterializeFailed,
                format!(
                    "failed to remove previous materialization '{}': {error}",
                    target_dir.display()
                ),
            )
        })?;
    }

    fs::rename(&tmp_dir, &target_dir).map_err(|error| {
        MbsrcError::new(
            ErrorClass::MaterializeFailed,
            format!(
                "failed to finalize materialization '{}' -> '{}': {error}",
                tmp_dir.display(),
                target_dir.display()
            ),
        )
    })?;

    // Apply read-only permissions after the final location is in place.
    make_read_only_recursive(&target_dir)?;

    update_shared_materialized_state(dirs, build, target_dir.as_path(), now)?;

    println!("materialize: ok");
    println!("artifact: {}", build.artifact);
    println!("commit: {}", build.commit);
    println!("target: {}", target_dir.display());
    Ok(())
}

fn run_dematerialize(dirs: &WorkspaceLayout, artifact_name: &str) -> MResult<()> {
    let target_dir = dirs.materialized.join(artifact_name);
    let mut shared: SharedState = parse_nickel_data_file(&dirs.shared_state)?;

    let had_materialized_state = shared
        .artifacts
        .iter()
        .find(|record| record.artifact == artifact_name)
        .map(|record| record.materialized)
        .unwrap_or(false);

    if target_dir.exists() {
        make_writable_recursive(&target_dir)?;
        fs::remove_dir_all(&target_dir).map_err(|error| {
            MbsrcError::new(
                ErrorClass::DematerializeFailed,
                format!(
                    "failed to remove materialized directory '{}': {error}",
                    target_dir.display()
                ),
            )
        })?;
    } else if !had_materialized_state {
        return Err(MbsrcError::new(
            ErrorClass::ArtifactNotFound,
            format!("artifact '{}' is not materialized", artifact_name),
        ));
    }

    if let Some(existing) = shared
        .artifacts
        .iter_mut()
        .find(|record| record.artifact == artifact_name)
    {
        existing.materialized = false;
        existing.materialized_path.clear();
        existing.materialized_at_epoch_secs = 0.0;
    }

    write_atomic(&dirs.shared_state, &render_shared_state_ncl(&shared))?;

    println!("dematerialize: ok");
    println!("artifact: {artifact_name}");
    println!("target: {}", target_dir.display());
    Ok(())
}

fn load_recipe_from_config(recipes_path: &Path, artifact_name: &str) -> MResult<Recipe> {
    if !recipes_path.exists() {
        return Err(MbsrcError::new(
            ErrorClass::ConfigNotFound,
            format!(
                "default recipes file '{}' was not found",
                recipes_path.display()
            ),
        ));
    }

    let json_value = eval_nickel_file_to_json(recipes_path)?;

    let object = json_value.as_object().ok_or_else(|| {
        MbsrcError::new(
            ErrorClass::InvalidRecipe,
            "Nickel config must export an object at top level",
        )
    })?;

    let artifact_value = object.get(artifact_name).ok_or_else(|| {
        MbsrcError::new(
            ErrorClass::ArtifactNotFound,
            format!(
                "artifact '{}' was not found in recipes '{}'",
                artifact_name,
                recipes_path.display()
            ),
        )
    })?;

    serde_json::from_value::<Recipe>(artifact_value.clone()).map_err(|error| {
        MbsrcError::new(
            ErrorClass::InvalidRecipe,
            format!("invalid recipe for artifact '{}': {error}", artifact_name),
        )
    })
}

fn eval_nickel_file_to_json(path: &Path) -> MResult<Value> {
    let source = fs::read_to_string(path).map_err(|error| {
        MbsrcError::new(
            ErrorClass::ConfigEvalFailed,
            format!("failed to read Nickel file '{}': {error}", path.display()),
        )
    })?;

    let import_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .as_os_str()
        .to_os_string();

    let mut context = NickelContext::new()
        .with_source_name(path.display().to_string())
        .with_added_import_paths(vec![import_dir]);

    let expr = context.eval_deep_for_export(&source).map_err(|error| {
        MbsrcError::new(ErrorClass::ConfigEvalFailed, format_nickel_error(error))
    })?;

    expr.to_serde().map_err(|error| {
        MbsrcError::new(
            ErrorClass::ConfigEvalFailed,
            format!("failed to deserialize evaluated Nickel value: {error}"),
        )
    })
}

fn parse_nickel_data_file<T>(path: &Path) -> MResult<T>
where
    T: DeserializeOwned + Default,
{
    if !path.exists() {
        return Ok(T::default());
    }

    let json_value = eval_nickel_file_to_json(path)?;
    serde_json::from_value::<T>(json_value).map_err(|error| {
        MbsrcError::new(
            ErrorClass::StateFailed,
            format!(
                "failed to decode '{}' as expected state type: {error}",
                path.display()
            ),
        )
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

fn validate_recipe(recipe: &Recipe) -> MResult<()> {
    if recipe.source.source_type != "github" {
        return Err(MbsrcError::new(
            ErrorClass::InvalidRecipe,
            "source.type must be 'github'",
        ));
    }

    parse_github_repo(&recipe.source.repo)?;

    if !is_valid_commit_hash(&recipe.source.commit) {
        return Err(MbsrcError::new(
            ErrorClass::InvalidRecipe,
            "source.commit must be a 40-character lowercase hex string",
        ));
    }

    Ok(())
}

fn parse_github_repo(repo: &str) -> MResult<(String, String)> {
    let stripped = if let Some(rest) = repo.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = repo.strip_prefix("git@github.com:") {
        rest
    } else {
        return Err(MbsrcError::new(
            ErrorClass::InvalidRecipe,
            "source.repo must be a GitHub URL",
        ));
    };

    let without_suffix = stripped.trim_end_matches(".git").trim_end_matches('/');
    let mut parts = without_suffix.split('/');

    let owner = parts.next().unwrap_or("");
    let repo_name = parts.next().unwrap_or("");

    if owner.is_empty() || repo_name.is_empty() || parts.next().is_some() {
        return Err(MbsrcError::new(
            ErrorClass::InvalidRecipe,
            "source.repo must be in owner/repo format",
        ));
    }

    Ok((owner.to_string(), repo_name.to_string()))
}

fn is_valid_commit_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn git_has_commit(mirror_path: &Path, commit: &str) -> MResult<bool> {
    let status = ProcessCommand::new("git")
        .arg("cat-file")
        .arg("-e")
        .arg(format!("{commit}^{{commit}}"))
        .current_dir(mirror_path)
        .status()
        .map_err(|error| {
            MbsrcError::new(
                ErrorClass::GitFailed,
                format!("failed to run git cat-file: {error}"),
            )
        })?;

    Ok(status.success())
}

fn run_git(args: &[&str], cwd: Option<&Path>, context: &str) -> MResult<()> {
    let mut command = ProcessCommand::new("git");
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = command.output().map_err(|error| {
        MbsrcError::new(
            ErrorClass::GitFailed,
            format!("{context}: failed to execute git: {error}"),
        )
    })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let details = if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        "git command failed without output".to_string()
    };

    Err(MbsrcError::new(
        ErrorClass::GitFailed,
        format!("{context}: {details}"),
    ))
}

fn run_command(
    command_name: &str,
    args: &[&str],
    cwd: Option<&Path>,
    context: &str,
) -> MResult<()> {
    let mut command = ProcessCommand::new(command_name);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = command.output().map_err(|error| {
        MbsrcError::new(
            ErrorClass::MaterializeFailed,
            format!("{context}: failed to execute {command_name}: {error}"),
        )
    })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let details = if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        format!("{command_name} failed without output")
    };

    Err(MbsrcError::new(
        ErrorClass::MaterializeFailed,
        format!("{context}: {details}"),
    ))
}

fn update_states(
    dirs: &WorkspaceLayout,
    artifact: &str,
    repo: &str,
    commit: &str,
    mirror_path: &Path,
) -> MResult<()> {
    let mut internal: InternalState = parse_nickel_data_file(&dirs.internal_state)?;
    let previous_shared: SharedState = parse_nickel_data_file(&dirs.shared_state)?;
    let now = current_epoch_secs()?;

    if let Some(existing) = internal
        .builds
        .iter_mut()
        .find(|record| record.artifact == artifact)
    {
        existing.repo = repo.to_string();
        existing.commit = commit.to_string();
        existing.mirror_path = mirror_path.to_string_lossy().to_string();
        existing.updated_at_epoch_secs = now;
    } else {
        internal.builds.push(BuildRecord {
            artifact: artifact.to_string(),
            repo: repo.to_string(),
            commit: commit.to_string(),
            mirror_path: mirror_path.to_string_lossy().to_string(),
            updated_at_epoch_secs: now,
        });
    }

    let existing_by_artifact: HashMap<String, SharedArtifactRecord> = previous_shared
        .artifacts
        .into_iter()
        .map(|record| (record.artifact.clone(), record))
        .collect();

    let shared = SharedState {
        artifacts: internal
            .builds
            .iter()
            .map(|record| {
                if let Some(existing) = existing_by_artifact.get(&record.artifact) {
                    // Preserve materialization metadata only if commit did not change.
                    if existing.commit == record.commit {
                        return SharedArtifactRecord {
                            artifact: record.artifact.clone(),
                            repo: record.repo.clone(),
                            commit: record.commit.clone(),
                            materialized: existing.materialized,
                            materialized_path: existing.materialized_path.clone(),
                            materialized_at_epoch_secs: existing.materialized_at_epoch_secs,
                            updated_at_epoch_secs: record.updated_at_epoch_secs,
                        };
                    }
                }

                SharedArtifactRecord {
                    artifact: record.artifact.clone(),
                    repo: record.repo.clone(),
                    commit: record.commit.clone(),
                    materialized: false,
                    materialized_path: String::new(),
                    materialized_at_epoch_secs: 0.0,
                    updated_at_epoch_secs: record.updated_at_epoch_secs,
                }
            })
            .collect(),
    };

    write_atomic(&dirs.internal_state, &render_internal_state_ncl(&internal))?;
    write_atomic(&dirs.shared_state, &render_shared_state_ncl(&shared))?;

    Ok(())
}

fn update_shared_materialized_state(
    dirs: &WorkspaceLayout,
    build: &BuildRecord,
    target_path: &Path,
    materialized_at: f64,
) -> MResult<()> {
    let mut shared: SharedState = parse_nickel_data_file(&dirs.shared_state)?;

    if let Some(existing) = shared
        .artifacts
        .iter_mut()
        .find(|record| record.artifact == build.artifact)
    {
        existing.repo = build.repo.clone();
        existing.commit = build.commit.clone();
        existing.updated_at_epoch_secs = build.updated_at_epoch_secs;
        existing.materialized = true;
        existing.materialized_path = target_path.to_string_lossy().to_string();
        existing.materialized_at_epoch_secs = materialized_at;
    } else {
        shared.artifacts.push(SharedArtifactRecord {
            artifact: build.artifact.clone(),
            repo: build.repo.clone(),
            commit: build.commit.clone(),
            materialized: true,
            materialized_path: target_path.to_string_lossy().to_string(),
            materialized_at_epoch_secs: materialized_at,
            updated_at_epoch_secs: build.updated_at_epoch_secs,
        });
    }

    write_atomic(&dirs.shared_state, &render_shared_state_ncl(&shared))
}

fn render_internal_state_ncl(state: &InternalState) -> String {
    let mut out = String::from("{\n  builds = [\n");
    for record in &state.builds {
        out.push_str("    {\n");
        out.push_str(&format!("      artifact = {},\n", q(&record.artifact)));
        out.push_str(&format!("      repo = {},\n", q(&record.repo)));
        out.push_str(&format!("      commit = {},\n", q(&record.commit)));
        out.push_str(&format!(
            "      mirror_path = {},\n",
            q(&record.mirror_path)
        ));
        out.push_str(&format!(
            "      updated_at_epoch_secs = {},\n",
            record.updated_at_epoch_secs
        ));
        out.push_str("    },\n");
    }
    out.push_str("  ],\n}\n");
    out
}

fn render_shared_state_ncl(state: &SharedState) -> String {
    let mut out = String::from("{\n  artifacts = [\n");
    for record in &state.artifacts {
        out.push_str("    {\n");
        out.push_str(&format!("      artifact = {},\n", q(&record.artifact)));
        out.push_str(&format!("      repo = {},\n", q(&record.repo)));
        out.push_str(&format!("      commit = {},\n", q(&record.commit)));
        out.push_str(&format!("      materialized = {},\n", record.materialized));
        out.push_str(&format!(
            "      materialized_path = {},\n",
            q(&record.materialized_path)
        ));
        out.push_str(&format!(
            "      materialized_at_epoch_secs = {},\n",
            record.materialized_at_epoch_secs
        ));
        out.push_str(&format!(
            "      updated_at_epoch_secs = {},\n",
            record.updated_at_epoch_secs
        ));
        out.push_str("    },\n");
    }
    out.push_str("  ],\n}\n");
    out
}

fn q(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<serialization-error>\"".to_string())
}

fn write_atomic(path: &Path, content: &str) -> MResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!(
                    "invalid file name for atomic write path '{}'",
                    path.display()
                ),
            )
        })?;

    let tmp_name = format!(".{file_name}.tmp");
    let tmp_path = path.with_file_name(tmp_name);

    fs::write(&tmp_path, content).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to write temporary file '{}': {error}",
                tmp_path.display()
            ),
        )
    })?;

    fs::rename(&tmp_path, path).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to move temporary file '{}' to '{}': {error}",
                tmp_path.display(),
                path.display()
            ),
        )
    })
}

fn current_epoch_secs() -> MResult<f64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|error| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!("system time before UNIX_EPOCH: {error}"),
            )
        })
}

struct WorkspaceLayout {
    root: PathBuf,
    recipes: PathBuf,
    shared_state: PathBuf,
    builder_root: PathBuf,
    internal_state: PathBuf,
    mirrors: PathBuf,
    materialized: PathBuf,
}

fn workspace_layout() -> WorkspaceLayout {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = cwd.join(ROOT_DIR);
    let builder_root = root.join(BUILDER_DIR);
    WorkspaceLayout {
        recipes: root.join(RECIPES_FILE),
        shared_state: root.join(SHARED_STATE_FILE),
        builder_root: builder_root.clone(),
        internal_state: builder_root.join(INTERNAL_STATE_FILE),
        mirrors: builder_root.join(MIRRORS_DIR),
        materialized: root.join(MATERIALIZED_DIR),
        root,
    }
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> MResult<()> {
    ensure_dir(&layout.root, "mbsrc root")?;
    ensure_dir(&layout.builder_root, "builder root")?;
    ensure_dir(&layout.materialized, "materialized")?;
    ensure_state_file(&layout.shared_state)?;
    Ok(())
}

fn ensure_dir(path: &PathBuf, label: &str) -> MResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to create or access {label} directory '{}': {error}",
                path.display()
            ),
        )
    })
}

fn ensure_state_file(path: &Path) -> MResult<()> {
    if path.exists() {
        return Ok(());
    }

    let default_state = render_shared_state_ncl(&SharedState::default());
    write_atomic(path, &default_state)
}

fn make_read_only_recursive(path: &Path) -> MResult<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path).map_err(|error| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!("failed to read directory '{}': {error}", path.display()),
            )
        })? {
            let entry = entry.map_err(|error| {
                MbsrcError::new(
                    ErrorClass::FsFailed,
                    format!(
                        "failed to read directory entry in '{}': {error}",
                        path.display()
                    ),
                )
            })?;
            make_read_only_recursive(&entry.path())?;
        }
        set_dir_read_only(path)?;
    } else if path.is_file() {
        set_file_read_only(path)?;
    } else {
        let _ = File::open(path);
    }

    Ok(())
}

fn make_writable_recursive(path: &Path) -> MResult<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path).map_err(|error| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!("failed to read directory '{}': {error}", path.display()),
            )
        })? {
            let entry = entry.map_err(|error| {
                MbsrcError::new(
                    ErrorClass::FsFailed,
                    format!(
                        "failed to read directory entry in '{}': {error}",
                        path.display()
                    ),
                )
            })?;
            make_writable_recursive(&entry.path())?;
        }
        set_dir_writable(path)?;
    } else if path.is_file() {
        set_file_writable(path)?;
    }

    Ok(())
}

#[cfg(unix)]
fn set_dir_read_only(path: &Path) -> MResult<()> {
    let perms = fs::Permissions::from_mode(0o555);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set read-only permissions on directory '{}': {error}",
                path.display()
            ),
        )
    })
}

#[cfg(unix)]
fn set_file_read_only(path: &Path) -> MResult<()> {
    let perms = fs::Permissions::from_mode(0o444);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set read-only permissions on file '{}': {error}",
                path.display()
            ),
        )
    })
}

#[cfg(unix)]
fn set_dir_writable(path: &Path) -> MResult<()> {
    let perms = fs::Permissions::from_mode(0o755);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set writable permissions on directory '{}': {error}",
                path.display()
            ),
        )
    })
}

#[cfg(unix)]
fn set_file_writable(path: &Path) -> MResult<()> {
    let perms = fs::Permissions::from_mode(0o644);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set writable permissions on file '{}': {error}",
                path.display()
            ),
        )
    })
}

#[cfg(not(unix))]
fn set_dir_read_only(path: &Path) -> MResult<()> {
    let mut perms = fs::metadata(path)
        .map_err(|error| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!("failed to read metadata for '{}': {error}", path.display()),
            )
        })?
        .permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set read-only permissions on directory '{}': {error}",
                path.display()
            ),
        )
    })
}

#[cfg(not(unix))]
fn set_file_read_only(path: &Path) -> MResult<()> {
    let mut perms = fs::metadata(path)
        .map_err(|error| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!("failed to read metadata for '{}': {error}", path.display()),
            )
        })?
        .permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set read-only permissions on file '{}': {error}",
                path.display()
            ),
        )
    })
}

#[cfg(not(unix))]
fn set_dir_writable(path: &Path) -> MResult<()> {
    let mut perms = fs::metadata(path)
        .map_err(|error| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!("failed to read metadata for '{}': {error}", path.display()),
            )
        })?
        .permissions();
    perms.set_readonly(false);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set writable permissions on directory '{}': {error}",
                path.display()
            ),
        )
    })
}

#[cfg(not(unix))]
fn set_file_writable(path: &Path) -> MResult<()> {
    let mut perms = fs::metadata(path)
        .map_err(|error| {
            MbsrcError::new(
                ErrorClass::FsFailed,
                format!("failed to read metadata for '{}': {error}", path.display()),
            )
        })?
        .permissions();
    perms.set_readonly(false);
    fs::set_permissions(path, perms).map_err(|error| {
        MbsrcError::new(
            ErrorClass::FsFailed,
            format!(
                "failed to set writable permissions on file '{}': {error}",
                path.display()
            ),
        )
    })
}
