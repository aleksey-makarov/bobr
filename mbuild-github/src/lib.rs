use mbuild_core::{Builder, BuilderError, VerbSpec};
use nickel_lang::{Context as NickelContext, ErrorFormat as NickelErrorFormat};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

const ROOT_DIR: &str = ".mbuild";
const SHARED_STATE_FILE: &str = "state.ncl";
const INTERNAL_STATE_FILE: &str = "internal.ncl";
const BUILDER_DIR: &str = "github";
const MIRRORS_DIR: &str = "mirrors";
const MATERIALIZED_DIR: &str = "materialized";
const CUSTOM_VERBS: &[VerbSpec] = &[VerbSpec {
    name: "cache",
    description: "populate or update github mirrors without materializing outputs",
}];

type MResult<T> = Result<T, MbsrcError>;

#[derive(Debug)]
enum MbsrcError {
    ConfigEvalFailed(String),
    ArtifactNotFound(String),
    InvalidRecipe(String),
    GitFailed(String),
    CommitNotFound(String),
    StateFailed(String),
    MaterializeFailed(String),
    FsFailed(String),
}

impl MbsrcError {
    fn message(&self) -> &str {
        match self {
            Self::ConfigEvalFailed(message)
            | Self::ArtifactNotFound(message)
            | Self::InvalidRecipe(message)
            | Self::GitFailed(message)
            | Self::CommitNotFound(message)
            | Self::StateFailed(message)
            | Self::MaterializeFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for MbsrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

#[derive(Debug, Deserialize)]
struct GithubRecipe {
    #[serde(rename = "type")]
    recipe_type: String,
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

pub struct GithubBuilder;

impl Builder for GithubBuilder {
    fn get_type(&self) -> &'static str {
        "github"
    }

    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        let recipe = parse_recipe(recipe)?;
        let dirs = workspace_layout();
        ensure_base_dirs(&dirs).map_err(map_error)?;
        run_cache(&dirs, artifact, &recipe).map_err(map_error)?;
        run_materialize(&dirs, artifact).map_err(map_error)
    }

    fn summarize_recipe(&self, recipe: &Value) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let recipe = parse_recipe(recipe)?;
        Ok(vec![
            ("repo", recipe.repo),
            ("commit", recipe.commit),
        ])
    }

    fn custom_verbs(&self) -> &'static [VerbSpec] {
        CUSTOM_VERBS
    }

    fn run_custom(&self, verb: &str, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        match verb {
            "cache" => {
                let recipe = parse_recipe(recipe)?;
                let dirs = workspace_layout();
                ensure_base_dirs(&dirs).map_err(map_error)?;
                run_cache(&dirs, artifact, &recipe).map_err(map_error)
            }
            _ => Err(BuilderError::UnsupportedVerb(format!(
                "verb '{}' is not supported by builder '{}' for artifact '{}'",
                verb,
                self.get_type(),
                artifact
            ))),
        }
    }
}

fn run_cache(dirs: &WorkspaceLayout, artifact_name: &str, recipe: &GithubRecipe) -> MResult<()> {
    ensure_dir(&dirs.mirrors, "mirrors")?;

    let (owner, repo_name) = parse_github_repo(&recipe.repo)?;
    let mirror_name = format!("{owner}_{repo_name}.git");
    let mirror_path = dirs.mirrors.join(mirror_name);

    if mirror_path.exists() {
        if !mirror_path.is_dir() {
            return Err(MbsrcError::FsFailed(
                format!(
                    "mirror path exists but is not a directory: {}",
                    mirror_path.display()
                ),
            ));
        }

        let has_commit = git_has_commit(&mirror_path, &recipe.commit)?;
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
                &recipe.repo,
                &mirror_path.to_string_lossy(),
            ],
            None,
            "failed to clone mirror",
        )?;
    }

    if !git_has_commit(&mirror_path, &recipe.commit)? {
        return Err(MbsrcError::CommitNotFound(
            format!(
                "commit {} not found in mirror {}",
                recipe.commit,
                mirror_path.display()
            ),
        ));
    }

    update_states(
        dirs,
        artifact_name,
        &recipe.repo,
        &recipe.commit,
        &mirror_path,
    )?;

    println!("cache: ok");
    println!("artifact: {artifact_name}");
    println!("repo: {}", recipe.repo);
    println!("commit: {}", recipe.commit);
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
            MbsrcError::ArtifactNotFound(
                format!(
                    "artifact '{}' is not cached yet; run `mbuild {} cache` first",
                    artifact_name, artifact_name
                ),
            )
        })?;

    let mirror_path = PathBuf::from(&build.mirror_path);
    if !mirror_path.is_dir() {
        return Err(MbsrcError::FsFailed(
            format!(
                "mirror directory does not exist for artifact '{}': {}",
                artifact_name,
                mirror_path.display()
            ),
        ));
    }

    if !git_has_commit(&mirror_path, &build.commit)? {
        return Err(MbsrcError::CommitNotFound(
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
            MbsrcError::MaterializeFailed(
                format!(
                    "failed to clean temporary directory '{}': {error}",
                    tmp_dir.display()
                ),
            )
        })?;
    }
    if tmp_tar.exists() {
        fs::remove_file(&tmp_tar).map_err(|error| {
            MbsrcError::MaterializeFailed(
                format!(
                    "failed to clean temporary tar '{}': {error}",
                    tmp_tar.display()
                ),
            )
        })?;
    }

    fs::create_dir_all(&tmp_dir).map_err(|error| {
        MbsrcError::MaterializeFailed(
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
            MbsrcError::MaterializeFailed(
                format!(
                    "failed to remove temporary tar '{}': {error}",
                    tmp_tar.display()
                ),
            )
        })?;
    }

    if target_dir.exists() {
        fs::remove_dir_all(&target_dir).map_err(|error| {
            MbsrcError::MaterializeFailed(
                format!(
                    "failed to remove previous materialization '{}': {error}",
                    target_dir.display()
                ),
            )
        })?;
    }

    fs::rename(&tmp_dir, &target_dir).map_err(|error| {
        MbsrcError::MaterializeFailed(
            format!(
                "failed to finalize materialization '{}' -> '{}': {error}",
                tmp_dir.display(),
                target_dir.display()
            ),
        )
    })?;

    update_shared_materialized_state(dirs, build, target_dir.as_path(), now)?;

    println!("materialize: ok");
    println!("artifact: {}", build.artifact);
    println!("commit: {}", build.commit);
    println!("target: {}", target_dir.display());
    Ok(())
}

fn eval_nickel_file_to_json(path: &Path) -> MResult<Value> {
    let source = fs::read_to_string(path).map_err(|error| {
        MbsrcError::ConfigEvalFailed(
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
        MbsrcError::ConfigEvalFailed( format_nickel_error(error))
    })?;

    expr.to_serde().map_err(|error| {
        MbsrcError::ConfigEvalFailed(
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
        MbsrcError::StateFailed(
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

fn validate_recipe(recipe: &GithubRecipe) -> MResult<()> {
    if recipe.recipe_type != "github" {
        return Err(MbsrcError::InvalidRecipe(
            "type must be 'github'".to_string(),
        ));
    }

    parse_github_repo(&recipe.repo)?;

    if !is_valid_commit_hash(&recipe.commit) {
        return Err(MbsrcError::InvalidRecipe(
            "commit must be a 40-character lowercase hex string".to_string(),
        ));
    }

    Ok(())
}

fn parse_recipe(value: &Value) -> Result<GithubRecipe, BuilderError> {
    serde_json::from_value::<GithubRecipe>(value.clone())
        .map_err(|error| BuilderError::InvalidRecipe(format!("invalid github recipe: {error}")))
        .and_then(|recipe| {
            validate_recipe(&recipe)
                .map_err(map_error)?;
            Ok(recipe)
        })
}

fn map_error(error: MbsrcError) -> BuilderError {
    match error {
        MbsrcError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        MbsrcError::ArtifactNotFound(message)
        | MbsrcError::ConfigEvalFailed(message)
        | MbsrcError::GitFailed(message)
        | MbsrcError::CommitNotFound(message)
        | MbsrcError::StateFailed(message)
        | MbsrcError::MaterializeFailed(message)
        | MbsrcError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}

fn parse_github_repo(repo: &str) -> MResult<(String, String)> {
    let stripped = if let Some(rest) = repo.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = repo.strip_prefix("git@github.com:") {
        rest
    } else {
        return Err(MbsrcError::InvalidRecipe(
            "source.repo must be a GitHub URL".to_string(),
        ));
    };

    let without_suffix = stripped.trim_end_matches(".git").trim_end_matches('/');
    let mut parts = without_suffix.split('/');

    let owner = parts.next().unwrap_or("");
    let repo_name = parts.next().unwrap_or("");

    if owner.is_empty() || repo_name.is_empty() || parts.next().is_some() {
        return Err(MbsrcError::InvalidRecipe(
            "source.repo must be in owner/repo format".to_string(),
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
            MbsrcError::GitFailed(
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
        MbsrcError::GitFailed(
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

    Err(MbsrcError::GitFailed(
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
        MbsrcError::MaterializeFailed(
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

    Err(MbsrcError::MaterializeFailed(
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
            MbsrcError::FsFailed(
                format!(
                    "invalid file name for atomic write path '{}'",
                    path.display()
                ),
            )
        })?;

    let tmp_name = format!(".{file_name}.tmp");
    let tmp_path = path.with_file_name(tmp_name);

    fs::write(&tmp_path, content).map_err(|error| {
        MbsrcError::FsFailed(
            format!(
                "failed to write temporary file '{}': {error}",
                tmp_path.display()
            ),
        )
    })?;

    fs::rename(&tmp_path, path).map_err(|error| {
        MbsrcError::FsFailed(
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
            MbsrcError::FsFailed(
                format!("system time before UNIX_EPOCH: {error}"),
            )
        })
}

struct WorkspaceLayout {
    root: PathBuf,
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
        MbsrcError::FsFailed(
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
