use mbuild_core::cas::{CasError, PublishOutputRequest, StoreLayout, publish_output};
use mbuild_core::{Builder, BuilderError, fsutil};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const ROOT_DIR: &str = ".mbuild";
const BUILDER_DIR: &str = "text";
const TMP_DIR: &str = "tmp";

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

    let now_nanos = fsutil::current_epoch_nanos().map_err(map_fsutil_error)?;
    let tmp_path = layout
        .tmp
        .join(format!("text-{}-{}.obj", output_name, now_nanos));

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
    #[cfg(unix)]
    if artifact_kind == "build-script" {
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&tmp_path, perms).map_err(|error| {
            TextError::PublishFailed(format!(
                "failed to set executable mode on build-script '{}': {error}",
                tmp_path.display()
            ))
        })?;
    }

    let mut attrs = Map::new();
    attrs.insert(
        "source_bytes".to_string(),
        Value::from(source_text.len() as u64),
    );

    let published = publish_output(
        &layout.store,
        PublishOutputRequest {
            output_name: output_name.to_string(),
            staged_path: tmp_path,
            artifact_kind: artifact_kind.to_string(),
            producer_builder: "text".to_string(),
            input_artifact_hashes: vec![],
            attrs,
        },
    )
    .map_err(map_cas_error)?;

    println!("publish: ok");
    println!("output: {output_name}");
    println!("source_bytes: {}", source_text.len());
    println!("object_hash: {}", published.object_hash);
    println!("artifact_hash: {}", published.artifact_hash);
    Ok(())
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

struct WorkspaceLayout {
    builder_root: PathBuf,
    tmp: PathBuf,
    store: StoreLayout,
}

fn workspace_layout() -> TResult<WorkspaceLayout> {
    let cwd = env::current_dir().map_err(|error| {
        TextError::FsFailed(format!("failed to get current directory: {error}"))
    })?;
    let root = cwd.join(ROOT_DIR);
    let builder_root = root.join(BUILDER_DIR);

    Ok(WorkspaceLayout {
        builder_root: builder_root.clone(),
        tmp: builder_root.join(TMP_DIR),
        store: StoreLayout::discover(&root).map_err(map_cas_error)?,
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> TResult<()> {
    ensure_dir(&layout.builder_root, "text builder root")?;
    ensure_dir(&layout.tmp, "text temp")?;
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

fn map_fsutil_error(error: fsutil::FsUtilError) -> TextError {
    TextError::FsFailed(error.to_string())
}

fn map_cas_error(error: CasError) -> TextError {
    TextError::PublishFailed(error.to_string())
}

fn map_error(error: TextError) -> BuilderError {
    match error {
        TextError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        TextError::FsFailed(message) | TextError::PublishFailed(message) => {
            BuilderError::ExecutionFailed(message)
        }
    }
}
