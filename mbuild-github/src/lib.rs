use mbuild_core::{Builder, BuilderError, VerbSpec};
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

const ROOT_DIR: &str = ".mbuild";
const BUILDER_DIR: &str = "github";
const MIRRORS_DIR: &str = "mirrors";
const OBJECTS_DIR: &str = "objects";
const META_DIR: &str = "meta";
const REFS_DIR: &str = "refs";
const CUSTOM_VERBS: &[VerbSpec] = &[VerbSpec {
    name: "cache",
    description: "populate or update github mirrors without publishing outputs",
}];

type GResult<T> = Result<T, GithubError>;

#[derive(Debug)]
enum GithubError {
    InvalidRecipe(String),
    GitFailed(String),
    CommitNotFound(String),
    PublishFailed(String),
    FsFailed(String),
}

impl GithubError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidRecipe(message)
            | Self::GitFailed(message)
            | Self::CommitNotFound(message)
            | Self::PublishFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for GithubError {
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
    #[serde(default)]
    outputs: Vec<String>,
}

pub struct GithubBuilder;

impl Builder for GithubBuilder {
    fn get_type(&self) -> &'static str {
        "github"
    }

    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        let recipe = parse_recipe(recipe)?;
        let layout = workspace_layout().map_err(map_error)?;
        ensure_base_dirs(&layout).map_err(map_error)?;

        let mirror_path = run_cache(&layout, artifact, &recipe).map_err(map_error)?;
        let outputs = output_ids(artifact, &recipe);
        for output_id in &outputs {
            publish_output(
                &layout,
                output_id,
                &recipe.repo,
                &recipe.commit,
                &mirror_path,
            )
            .map_err(map_error)?;
        }

        println!("build: ok");
        println!("artifact: {artifact}");
        println!("outputs: {}", outputs.join(", "));
        Ok(())
    }

    fn summarize_recipe(
        &self,
        recipe: &Value,
    ) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let recipe = parse_recipe(recipe)?;
        Ok(vec![("repo", recipe.repo), ("commit", recipe.commit)])
    }

    fn custom_verbs(&self) -> &'static [VerbSpec] {
        CUSTOM_VERBS
    }

    fn run_custom(&self, verb: &str, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        match verb {
            "cache" => {
                let recipe = parse_recipe(recipe)?;
                let layout = workspace_layout().map_err(map_error)?;
                ensure_base_dirs(&layout).map_err(map_error)?;
                let _ = run_cache(&layout, artifact, &recipe).map_err(map_error)?;
                Ok(())
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

fn output_ids(artifact: &str, recipe: &GithubRecipe) -> Vec<String> {
    if recipe.outputs.is_empty() {
        vec![artifact.to_string()]
    } else {
        recipe.outputs.clone()
    }
}

fn parse_recipe(value: &Value) -> Result<GithubRecipe, BuilderError> {
    serde_json::from_value::<GithubRecipe>(value.clone())
        .map_err(|error| BuilderError::InvalidRecipe(format!("invalid github recipe: {error}")))
        .and_then(|recipe| {
            validate_recipe(&recipe).map_err(map_error)?;
            Ok(recipe)
        })
}

fn validate_recipe(recipe: &GithubRecipe) -> GResult<()> {
    if recipe.recipe_type != "github" {
        return Err(GithubError::InvalidRecipe(
            "type must be 'github'".to_string(),
        ));
    }

    parse_github_repo(&recipe.repo)?;

    if !is_valid_commit_hash(&recipe.commit) {
        return Err(GithubError::InvalidRecipe(
            "commit must be a 40-character lowercase hex string".to_string(),
        ));
    }

    for output in &recipe.outputs {
        validate_artifact_name(output)?;
    }

    Ok(())
}

fn run_cache(
    layout: &WorkspaceLayout,
    artifact_name: &str,
    recipe: &GithubRecipe,
) -> GResult<PathBuf> {
    let (owner, repo_name) = parse_github_repo(&recipe.repo)?;
    let mirror_name = format!("{owner}_{repo_name}.git");
    let mirror_path = layout.mirrors.join(mirror_name);

    if mirror_path.exists() {
        if !mirror_path.is_dir() {
            return Err(GithubError::FsFailed(format!(
                "mirror path exists but is not a directory: {}",
                mirror_path.display()
            )));
        }

        if !git_has_commit(&mirror_path, &recipe.commit)? {
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
        return Err(GithubError::CommitNotFound(format!(
            "commit {} not found in mirror {}",
            recipe.commit,
            mirror_path.display()
        )));
    }

    println!("cache: ok");
    println!("artifact: {artifact_name}");
    println!("repo: {}", recipe.repo);
    println!("commit: {}", recipe.commit);
    println!("mirror: {}", mirror_path.display());

    Ok(mirror_path)
}

fn publish_output(
    layout: &WorkspaceLayout,
    output_id: &str,
    repo: &str,
    commit: &str,
    mirror_path: &Path,
) -> GResult<()> {
    validate_artifact_name(output_id)?;

    if !git_has_commit(mirror_path, commit)? {
        return Err(GithubError::CommitNotFound(format!(
            "commit {} not found in mirror {}",
            commit,
            mirror_path.display()
        )));
    }

    let now_nanos = current_epoch_nanos()?;
    let tmp_base = layout
        .root
        .join(format!(".publish-{}-{}", output_id, now_nanos));
    let tmp_dir = tmp_base.with_extension("dir");
    let tmp_tar = tmp_base.with_extension("tar");

    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir).map_err(|error| {
            GithubError::PublishFailed(format!(
                "failed to clean temporary directory '{}': {error}",
                tmp_dir.display()
            ))
        })?;
    }
    if tmp_tar.exists() {
        fs::remove_file(&tmp_tar).map_err(|error| {
            GithubError::PublishFailed(format!(
                "failed to clean temporary archive '{}': {error}",
                tmp_tar.display()
            ))
        })?;
    }

    fs::create_dir_all(&tmp_dir).map_err(|error| {
        GithubError::PublishFailed(format!(
            "failed to create temporary directory '{}': {error}",
            tmp_dir.display()
        ))
    })?;

    run_git(
        &[
            "archive",
            "--format=tar",
            "--output",
            &tmp_tar.to_string_lossy(),
            commit,
        ],
        Some(mirror_path),
        "failed to create archive from mirror",
    )?;

    run_command(
        "tar",
        &[
            "-xf",
            &tmp_tar.to_string_lossy(),
            "-C",
            &tmp_dir.to_string_lossy(),
        ],
        None,
        "failed to extract archive",
    )?;

    if tmp_tar.exists() {
        fs::remove_file(&tmp_tar).map_err(|error| {
            GithubError::PublishFailed(format!(
                "failed to remove temporary archive '{}': {error}",
                tmp_tar.display()
            ))
        })?;
    }

    let object_path = layout.objects.join(output_id);
    replace_dir(&tmp_dir, &object_path)?;

    let meta_path = layout.meta.join(format!("{output_id}.ncl"));
    write_atomic(
        &meta_path,
        &render_meta_ncl(output_id, "source-tree", repo, commit),
    )?;

    let ref_path = layout.refs.join(output_id);
    let ref_target = PathBuf::from("..").join(OBJECTS_DIR).join(output_id);
    replace_symlink(&ref_target, &ref_path)?;

    println!("publish: ok");
    println!("output: {output_id}");
    println!("object: {}", object_path.display());
    println!("meta: {}", meta_path.display());
    println!("ref: {}", ref_path.display());

    Ok(())
}

fn render_meta_ncl(id: &str, artifact_kind: &str, repo: &str, commit: &str) -> String {
    format!(
        "{{\n  id = {},\n  artifact_kind = {},\n  producer = {{\n    builder = \"github\",\n    repo = {},\n    commit = {},\n  }},\n  attrs = {{}},\n}}\n",
        q(id),
        q(artifact_kind),
        q(repo),
        q(commit)
    )
}

fn replace_dir(tmp_dir: &Path, destination: &Path) -> GResult<()> {
    if destination.exists() {
        if destination.is_dir() {
            fs::remove_dir_all(destination).map_err(|error| {
                GithubError::PublishFailed(format!(
                    "failed to remove previous object directory '{}': {error}",
                    destination.display()
                ))
            })?;
        } else {
            fs::remove_file(destination).map_err(|error| {
                GithubError::PublishFailed(format!(
                    "failed to remove previous object file '{}': {error}",
                    destination.display()
                ))
            })?;
        }
    }

    fs::rename(tmp_dir, destination).map_err(|error| {
        GithubError::PublishFailed(format!(
            "failed to publish object '{}' -> '{}': {error}",
            tmp_dir.display(),
            destination.display()
        ))
    })
}

fn replace_symlink(target: &Path, link_path: &Path) -> GResult<()> {
    if link_path.exists() || link_path.is_symlink() {
        let metadata = fs::symlink_metadata(link_path).map_err(|error| {
            GithubError::FsFailed(format!(
                "failed to inspect existing ref '{}': {error}",
                link_path.display()
            ))
        })?;

        if metadata.file_type().is_dir() {
            fs::remove_dir_all(link_path).map_err(|error| {
                GithubError::FsFailed(format!(
                    "failed to remove existing ref directory '{}': {error}",
                    link_path.display()
                ))
            })?;
        } else {
            fs::remove_file(link_path).map_err(|error| {
                GithubError::FsFailed(format!(
                    "failed to remove existing ref '{}': {error}",
                    link_path.display()
                ))
            })?;
        }
    }

    create_symlink(target, link_path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> GResult<()> {
    unix_fs::symlink(target, link_path).map_err(|error| {
        GithubError::FsFailed(format!(
            "failed to create ref symlink '{}' -> '{}': {error}",
            link_path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link_path: &Path) -> GResult<()> {
    Err(GithubError::FsFailed(
        "symlink refs are currently supported only on unix hosts".to_string(),
    ))
}

fn parse_github_repo(repo: &str) -> GResult<(String, String)> {
    let stripped = if let Some(rest) = repo.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = repo.strip_prefix("git@github.com:") {
        rest
    } else {
        return Err(GithubError::InvalidRecipe(
            "repo must be a GitHub URL".to_string(),
        ));
    };

    let without_suffix = stripped.trim_end_matches(".git").trim_end_matches('/');
    let mut parts = without_suffix.split('/');

    let owner = parts.next().unwrap_or("");
    let repo_name = parts.next().unwrap_or("");

    if owner.is_empty() || repo_name.is_empty() || parts.next().is_some() {
        return Err(GithubError::InvalidRecipe(
            "repo must be in owner/repo format".to_string(),
        ));
    }

    Ok((owner.to_string(), repo_name.to_string()))
}

fn validate_artifact_name(name: &str) -> GResult<()> {
    if name.is_empty() {
        return Err(GithubError::InvalidRecipe(
            "artifact/output name must not be empty".to_string(),
        ));
    }

    if name == "." || name == ".." {
        return Err(GithubError::InvalidRecipe(format!(
            "invalid artifact/output name '{name}'"
        )));
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(GithubError::InvalidRecipe(format!(
            "invalid artifact/output name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }

    Ok(())
}

fn is_valid_commit_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn git_has_commit(mirror_path: &Path, commit: &str) -> GResult<bool> {
    let status = ProcessCommand::new("git")
        .arg("cat-file")
        .arg("-e")
        .arg(format!("{commit}^{{commit}}"))
        .current_dir(mirror_path)
        .status()
        .map_err(|error| GithubError::GitFailed(format!("failed to run git cat-file: {error}")))?;

    Ok(status.success())
}

fn run_git(args: &[&str], cwd: Option<&Path>, context: &str) -> GResult<()> {
    let mut command = ProcessCommand::new("git");
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = command.output().map_err(|error| {
        GithubError::GitFailed(format!("{context}: failed to execute git: {error}"))
    })?;

    if output.status.success() {
        return Ok(());
    }

    Err(GithubError::GitFailed(format!(
        "{context}: {}",
        command_details(&output, "git command failed without output")
    )))
}

fn run_command(
    command_name: &str,
    args: &[&str],
    cwd: Option<&Path>,
    context: &str,
) -> GResult<()> {
    let mut command = ProcessCommand::new(command_name);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = command.output().map_err(|error| {
        GithubError::PublishFailed(format!(
            "{context}: failed to execute {command_name}: {error}"
        ))
    })?;

    if output.status.success() {
        return Ok(());
    }

    Err(GithubError::PublishFailed(format!(
        "{context}: {}",
        command_details(&output, &format!("{command_name} failed without output"))
    )))
}

fn command_details(output: &std::process::Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        fallback.to_string()
    }
}

fn q(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<serialization-error>\"".to_string())
}

fn write_atomic(path: &Path, content: &str) -> GResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            GithubError::FsFailed(format!(
                "invalid file name for atomic write path '{}'",
                path.display()
            ))
        })?;

    let tmp_name = format!(".{file_name}.tmp");
    let tmp_path = path.with_file_name(tmp_name);

    fs::write(&tmp_path, content).map_err(|error| {
        GithubError::FsFailed(format!(
            "failed to write temporary file '{}': {error}",
            tmp_path.display()
        ))
    })?;

    fs::rename(&tmp_path, path).map_err(|error| {
        GithubError::FsFailed(format!(
            "failed to move temporary file '{}' to '{}': {error}",
            tmp_path.display(),
            path.display()
        ))
    })
}

fn current_epoch_nanos() -> GResult<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| GithubError::FsFailed(format!("system time before UNIX_EPOCH: {error}")))
}

struct WorkspaceLayout {
    root: PathBuf,
    builder_root: PathBuf,
    mirrors: PathBuf,
    objects: PathBuf,
    meta: PathBuf,
    refs: PathBuf,
}

fn workspace_layout() -> GResult<WorkspaceLayout> {
    let cwd = env::current_dir().map_err(|error| {
        GithubError::FsFailed(format!("failed to get current directory: {error}"))
    })?;
    let root = cwd.join(ROOT_DIR);
    let builder_root = root.join(BUILDER_DIR);

    Ok(WorkspaceLayout {
        root: root.clone(),
        builder_root: builder_root.clone(),
        mirrors: builder_root.join(MIRRORS_DIR),
        objects: root.join(OBJECTS_DIR),
        meta: root.join(META_DIR),
        refs: root.join(REFS_DIR),
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> GResult<()> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.builder_root, "github builder root")?;
    ensure_dir(&layout.mirrors, "github mirrors")?;
    ensure_dir(&layout.objects, "objects")?;
    ensure_dir(&layout.meta, "meta")?;
    ensure_dir(&layout.refs, "refs")?;
    Ok(())
}

fn ensure_dir(path: &Path, label: &str) -> GResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        GithubError::FsFailed(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

fn map_error(error: GithubError) -> BuilderError {
    match error {
        GithubError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        GithubError::GitFailed(message)
        | GithubError::CommitNotFound(message)
        | GithubError::PublishFailed(message)
        | GithubError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}
