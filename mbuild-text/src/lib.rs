use mbuild_core::{Builder, BuilderError};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ROOT_DIR: &str = ".mbuild";
const OBJECTS_DIR: &str = "objects";
const META_DIR: &str = "meta";
const REFS_DIR: &str = "refs";

#[derive(Debug)]
enum TextError {
    InvalidRecipe(String),
    FsFailed(String),
    PublishFailed(String),
}

impl TextError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidRecipe(m) | Self::FsFailed(m) | Self::PublishFailed(m) => m,
        }
    }
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

type TResult<T> = Result<T, TextError>;

#[derive(Debug, Deserialize)]
struct TextRecipe {
    #[serde(rename = "type")]
    recipe_type: String,
    artifact_kind: String,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    sources: HashMap<String, String>,
}

pub struct TextBuilder;

impl Builder for TextBuilder {
    fn get_type(&self) -> &'static str {
        "text"
    }

    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        let recipe = parse_recipe(recipe)?;
        let layout = workspace_layout().map_err(map_error)?;
        ensure_base_dirs(&layout).map_err(map_error)?;

        let outputs = resolve_outputs(artifact, &recipe).map_err(map_error)?;

        for (output_name, source_text) in outputs {
            publish_one(&layout, &output_name, &recipe.artifact_kind, &source_text)
                .map_err(map_error)?;
        }

        println!("build: ok");
        println!("artifact: {artifact}");
        println!("type: text");
        Ok(())
    }

    fn summarize_recipe(
        &self,
        recipe: &Value,
    ) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let recipe = parse_recipe(recipe)?;
        Ok(vec![("artifact_kind", recipe.artifact_kind)])
    }
}

fn parse_recipe(value: &Value) -> Result<TextRecipe, BuilderError> {
    serde_json::from_value::<TextRecipe>(value.clone())
        .map_err(|error| BuilderError::InvalidRecipe(format!("invalid text recipe: {error}")))
        .and_then(|recipe| {
            validate_recipe(&recipe).map_err(map_error)?;
            Ok(recipe)
        })
}

fn validate_recipe(recipe: &TextRecipe) -> TResult<()> {
    if recipe.recipe_type != "text" {
        return Err(TextError::InvalidRecipe("type must be 'text'".to_string()));
    }

    if recipe.artifact_kind.is_empty() {
        return Err(TextError::InvalidRecipe(
            "artifact_kind must not be empty".to_string(),
        ));
    }

    if recipe.source.is_some() == !recipe.sources.is_empty() {
        return Err(TextError::InvalidRecipe(
            "exactly one of 'source' or 'sources' must be provided".to_string(),
        ));
    }

    for output in &recipe.outputs {
        validate_name(output)?;
    }

    for output in recipe.sources.keys() {
        validate_name(output)?;
    }

    Ok(())
}

fn resolve_outputs(artifact: &str, recipe: &TextRecipe) -> TResult<Vec<(String, String)>> {
    if let Some(source) = &recipe.source {
        let output_name = if recipe.outputs.is_empty() {
            artifact.to_string()
        } else if recipe.outputs.len() == 1 {
            recipe.outputs[0].clone()
        } else {
            return Err(TextError::InvalidRecipe(
                "when 'source' is used, outputs must contain at most one name".to_string(),
            ));
        };

        validate_name(&output_name)?;
        return Ok(vec![(output_name, source.clone())]);
    }

    let outputs = if recipe.outputs.is_empty() {
        return Err(TextError::InvalidRecipe(
            "'outputs' must be provided when 'sources' is used".to_string(),
        ));
    } else {
        recipe.outputs.clone()
    };

    let output_set: HashSet<&str> = outputs.iter().map(String::as_str).collect();
    let source_set: HashSet<&str> = recipe.sources.keys().map(String::as_str).collect();

    if output_set != source_set {
        return Err(TextError::InvalidRecipe(
            "outputs must exactly match keys in 'sources'".to_string(),
        ));
    }

    Ok(outputs
        .into_iter()
        .map(|output| {
            let src = recipe
                .sources
                .get(&output)
                .cloned()
                .unwrap_or_else(String::new);
            (output, src)
        })
        .collect())
}

fn publish_one(
    layout: &WorkspaceLayout,
    output_name: &str,
    artifact_kind: &str,
    source_text: &str,
) -> TResult<()> {
    validate_name(output_name)?;

    let now_nanos = current_epoch_nanos()?;
    let tmp_path = layout
        .root
        .join(format!(".tmp-text-{}-{}.obj", output_name, now_nanos));

    if tmp_path.exists() {
        fs::remove_file(&tmp_path).map_err(|error| {
            TextError::FsFailed(format!(
                "failed to remove previous temporary file '{}': {error}",
                tmp_path.display()
            ))
        })?;
    }

    fs::write(&tmp_path, source_text).map_err(|error| {
        TextError::PublishFailed(format!(
            "failed to write object payload '{}': {error}",
            tmp_path.display()
        ))
    })?;

    let object_path = layout.objects.join(output_name);
    replace_path(&tmp_path, &object_path)?;

    let meta_path = layout.meta.join(format!("{output_name}.ncl"));
    write_atomic(
        &meta_path,
        &render_meta_ncl(output_name, artifact_kind, source_text.len()),
    )?;

    let ref_path = layout.refs.join(output_name);
    let ref_target = PathBuf::from("..").join(OBJECTS_DIR).join(output_name);
    replace_symlink(&ref_target, &ref_path)?;

    println!("publish: ok");
    println!("output: {output_name}");
    println!("source_bytes: {}", source_text.len());
    println!("object: {}", object_path.display());
    Ok(())
}

fn render_meta_ncl(id: &str, artifact_kind: &str, source_bytes: usize) -> String {
    format!(
        "{{\n  id = {},\n  artifact_kind = {},\n  producer = {{\n    builder = \"text\",\n  }},\n  attrs = {{\n    source_bytes = {},\n  }},\n}}\n",
        q(id),
        q(artifact_kind),
        source_bytes
    )
}

fn validate_name(name: &str) -> TResult<()> {
    if name.is_empty() {
        return Err(TextError::InvalidRecipe(
            "name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(TextError::InvalidRecipe(format!("invalid name '{name}'")));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(TextError::InvalidRecipe(format!(
            "invalid name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

fn replace_path(tmp_path: &Path, destination: &Path) -> TResult<()> {
    if destination.exists() {
        if destination.is_dir() {
            fs::remove_dir_all(destination).map_err(|error| {
                TextError::PublishFailed(format!(
                    "failed to remove previous object directory '{}': {error}",
                    destination.display()
                ))
            })?;
        } else {
            fs::remove_file(destination).map_err(|error| {
                TextError::PublishFailed(format!(
                    "failed to remove previous object file '{}': {error}",
                    destination.display()
                ))
            })?;
        }
    }

    fs::rename(tmp_path, destination).map_err(|error| {
        TextError::PublishFailed(format!(
            "failed to publish object '{}' -> '{}': {error}",
            tmp_path.display(),
            destination.display()
        ))
    })
}

fn replace_symlink(target: &Path, link_path: &Path) -> TResult<()> {
    if link_path.exists() || link_path.is_symlink() {
        let metadata = fs::symlink_metadata(link_path).map_err(|error| {
            TextError::FsFailed(format!(
                "failed to inspect existing ref '{}': {error}",
                link_path.display()
            ))
        })?;

        if metadata.file_type().is_dir() {
            fs::remove_dir_all(link_path).map_err(|error| {
                TextError::FsFailed(format!(
                    "failed to remove existing ref directory '{}': {error}",
                    link_path.display()
                ))
            })?;
        } else {
            fs::remove_file(link_path).map_err(|error| {
                TextError::FsFailed(format!(
                    "failed to remove existing ref '{}': {error}",
                    link_path.display()
                ))
            })?;
        }
    }

    create_symlink(target, link_path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> TResult<()> {
    unix_fs::symlink(target, link_path).map_err(|error| {
        TextError::FsFailed(format!(
            "failed to create ref symlink '{}' -> '{}': {error}",
            link_path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link_path: &Path) -> TResult<()> {
    Err(TextError::FsFailed(
        "symlink refs are currently supported only on unix hosts".to_string(),
    ))
}

fn write_atomic(path: &Path, content: &str) -> TResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            TextError::FsFailed(format!(
                "invalid file name for atomic write path '{}'",
                path.display()
            ))
        })?;

    let tmp_name = format!(".{file_name}.tmp");
    let tmp_path = path.with_file_name(tmp_name);

    fs::write(&tmp_path, content).map_err(|error| {
        TextError::FsFailed(format!(
            "failed to write temporary file '{}': {error}",
            tmp_path.display()
        ))
    })?;

    fs::rename(&tmp_path, path).map_err(|error| {
        TextError::FsFailed(format!(
            "failed to move temporary file '{}' to '{}': {error}",
            tmp_path.display(),
            path.display()
        ))
    })
}

fn current_epoch_nanos() -> TResult<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| TextError::FsFailed(format!("system time before UNIX_EPOCH: {error}")))
}

fn q(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<serialization-error>\"".to_string())
}

struct WorkspaceLayout {
    root: PathBuf,
    objects: PathBuf,
    meta: PathBuf,
    refs: PathBuf,
}

fn workspace_layout() -> TResult<WorkspaceLayout> {
    let cwd = env::current_dir().map_err(|error| {
        TextError::FsFailed(format!("failed to get current directory: {error}"))
    })?;
    let root = cwd.join(ROOT_DIR);

    Ok(WorkspaceLayout {
        root: root.clone(),
        objects: root.join(OBJECTS_DIR),
        meta: root.join(META_DIR),
        refs: root.join(REFS_DIR),
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> TResult<()> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.objects, "objects")?;
    ensure_dir(&layout.meta, "meta")?;
    ensure_dir(&layout.refs, "refs")?;
    Ok(())
}

fn ensure_dir(path: &Path, label: &str) -> TResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        TextError::FsFailed(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

fn map_error(error: TextError) -> BuilderError {
    match error {
        TextError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        TextError::FsFailed(message) | TextError::PublishFailed(message) => {
            BuilderError::ExecutionFailed(message)
        }
    }
}
