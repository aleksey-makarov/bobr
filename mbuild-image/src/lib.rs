use mbuild_core::{Builder, BuilderError, fsutil};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;

const ROOT_DIR: &str = ".mbuild";
const OBJECTS_DIR: &str = "objects";
const META_DIR: &str = "meta";
const REFS_DIR: &str = "refs";
const BUILDER_PREFIX: &str = "mbuild-image";

const KIND_BINARY_OUTPUT: &str = "binary-output";
const KIND_CONTAINER_IMAGE: &str = "container-image";

type IResult<T> = Result<T, ImageError>;

#[derive(Debug)]
enum ImageError {
    InvalidRecipe(String),
    InputResolutionFailed(String),
    BuildFailed(String),
    PublishFailed(String),
    FsFailed(String),
}

#[derive(Debug, Deserialize)]
struct ImageRecipe {
    #[serde(rename = "type")]
    recipe_type: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug)]
struct ResolvedMeta {
    id: String,
    artifact_kind: String,
}

#[derive(Debug)]
struct ResolvedInput {
    name: String,
    id: String,
    object_path: PathBuf,
    artifact_kind: String,
}

#[derive(Debug)]
struct WorkspaceLayout {
    root: PathBuf,
    objects: PathBuf,
    meta: PathBuf,
    refs: PathBuf,
}

pub struct ImageBuilder;

impl Builder for ImageBuilder {
    fn get_type(&self) -> &'static str {
        "image"
    }

    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        let recipe = parse_recipe(recipe)?;
        let layout = workspace_layout().map_err(map_error)?;
        ensure_base_dirs(&layout).map_err(map_error)?;

        let outputs = resolve_outputs(artifact, &recipe).map_err(map_error)?;
        if outputs.len() != 1 {
            return Err(BuilderError::InvalidRecipe(
                "image builder requires exactly one output".to_string(),
            ));
        }
        let output_id = outputs[0].clone();

        let inputs = resolve_inputs(&layout, &recipe).map_err(map_error)?;
        let base_count = inputs
            .iter()
            .filter(|input| input.artifact_kind == KIND_CONTAINER_IMAGE)
            .count();
        let binary_inputs: Vec<&ResolvedInput> = inputs
            .iter()
            .filter(|input| input.artifact_kind == KIND_BINARY_OUTPUT)
            .collect();

        if base_count > 1 {
            return Err(BuilderError::InvalidRecipe(
                "image recipe must have at most one 'container-image' input".to_string(),
            ));
        }

        if binary_inputs.is_empty() {
            return Err(BuilderError::InvalidRecipe(
                "image recipe must have at least one 'binary-output' input".to_string(),
            ));
        }

        if inputs.iter().any(|input| {
            input.artifact_kind != KIND_BINARY_OUTPUT && input.artifact_kind != KIND_CONTAINER_IMAGE
        }) {
            return Err(BuilderError::InvalidRecipe(
                "image recipe supports only 'container-image' and 'binary-output' inputs"
                    .to_string(),
            ));
        }

        if base_count == 1 {
            return Err(BuilderError::NotImplemented(
                "layered mode is not implemented yet; bootstrap mode (no base image input) is available"
                    .to_string(),
            ));
        }

        let imported = run_bootstrap_mode(artifact, &binary_inputs).map_err(map_error)?;
        publish_output(&layout, &output_id, &binary_inputs, &imported).map_err(map_error)?;

        println!("build: ok");
        println!("artifact: {artifact}");
        println!("output: {output_id}");
        println!("mode: bootstrap");
        println!("image_ref: {}", imported.image_ref);
        if !imported.image_digest.is_empty() {
            println!("image_digest: {}", imported.image_digest);
        }

        Ok(())
    }

    fn summarize_recipe(
        &self,
        recipe: &Value,
    ) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let recipe = parse_recipe(recipe)?;
        Ok(vec![
            ("inputs", recipe.inputs.len().to_string()),
            ("outputs", recipe.outputs.len().to_string()),
        ])
    }
}

#[derive(Debug)]
struct ImportedImage {
    image_ref: String,
    image_id: String,
    image_digest: String,
}

fn run_bootstrap_mode(artifact: &str, binary_inputs: &[&ResolvedInput]) -> IResult<ImportedImage> {
    let temp_base = fsutil::temp_root_dir(ROOT_DIR).map_err(map_fsutil_error)?;
    let now = fsutil::current_epoch_nanos().map_err(map_fsutil_error)?;
    let temp_root = temp_base.join(format!("image-bootstrap-{artifact}-{now}"));
    let rootfs_dir = temp_root.join("rootfs");
    let tar_path = temp_root.join("rootfs.tar");

    fsutil::recreate_empty_dir_force(&temp_root).map_err(map_fsutil_error)?;
    recreate_empty_dir(&rootfs_dir)?;

    let build_result = (|| {
        let mut seen: HashMap<PathBuf, EntryKind> = HashMap::new();
        for input in binary_inputs {
            copy_input_tree(input, &rootfs_dir, &mut seen)?;
        }

        create_rootfs_tar(&rootfs_dir, &tar_path)?;

        let image_ref = format!("localhost/{BUILDER_PREFIX}:{artifact}-{now}");
        let image_id = podman_import(&tar_path, &image_ref)?;
        let image_digest = podman_image_digest(&image_ref)?;

        Ok(ImportedImage {
            image_ref,
            image_id,
            image_digest,
        })
    })();

    let cleanup_result = fsutil::remove_dir_force(&temp_root).map_err(map_fsutil_error);
    match (build_result, cleanup_result) {
        (Ok(imported), Ok(())) => Ok(imported),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(_cleanup_error)) => Err(error),
    }
}

fn resolve_outputs(artifact: &str, recipe: &ImageRecipe) -> IResult<Vec<String>> {
    if recipe.outputs.is_empty() {
        return Ok(vec![artifact.to_string()]);
    }

    if recipe.outputs.len() != 1 {
        return Err(ImageError::InvalidRecipe(
            "image recipe outputs must contain exactly one name".to_string(),
        ));
    }

    validate_name(&recipe.outputs[0])?;
    Ok(recipe.outputs.clone())
}

fn copy_input_tree(
    input: &ResolvedInput,
    rootfs_dir: &Path,
    seen: &mut HashMap<PathBuf, EntryKind>,
) -> IResult<()> {
    copy_dir_recursive(
        &input.object_path,
        &input.object_path,
        rootfs_dir,
        seen,
        &input.name,
    )
}

fn copy_dir_recursive(
    root: &Path,
    current: &Path,
    destination_root: &Path,
    seen: &mut HashMap<PathBuf, EntryKind>,
    source_name: &str,
) -> IResult<()> {
    for entry in fs::read_dir(current).map_err(|error| {
        ImageError::BuildFailed(format!(
            "failed to read directory '{}' from input '{}': {error}",
            current.display(),
            source_name
        ))
    })? {
        let entry = entry.map_err(|error| {
            ImageError::BuildFailed(format!(
                "failed to read directory entry in '{}' from input '{}': {error}",
                current.display(),
                source_name
            ))
        })?;
        let path = entry.path();
        let rel = path.strip_prefix(root).map_err(|error| {
            ImageError::BuildFailed(format!(
                "failed to compute relative path for '{}' in input '{}': {error}",
                path.display(),
                source_name
            ))
        })?;

        if rel.as_os_str().is_empty() {
            continue;
        }

        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            ImageError::BuildFailed(format!(
                "failed to stat '{}' from input '{}': {error}",
                path.display(),
                source_name
            ))
        })?;

        let file_type = metadata.file_type();
        let rel_path = rel.to_path_buf();
        let target_path = destination_root.join(&rel_path);

        if file_type.is_dir() {
            register_path(seen, &rel_path, EntryKind::Directory, source_name)?;
            fs::create_dir_all(&target_path).map_err(|error| {
                ImageError::BuildFailed(format!(
                    "failed to create target directory '{}': {error}",
                    target_path.display()
                ))
            })?;
            copy_dir_recursive(root, &path, destination_root, seen, source_name)?;
            continue;
        }

        if file_type.is_file() {
            register_path(seen, &rel_path, EntryKind::File, source_name)?;
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    ImageError::BuildFailed(format!(
                        "failed to create parent directory '{}': {error}",
                        parent.display()
                    ))
                })?;
            }
            fs::copy(&path, &target_path).map_err(|error| {
                ImageError::BuildFailed(format!(
                    "failed to copy '{}' to '{}': {error}",
                    path.display(),
                    target_path.display()
                ))
            })?;
            fs::set_permissions(&target_path, metadata.permissions()).map_err(|error| {
                ImageError::BuildFailed(format!(
                    "failed to set permissions on '{}': {error}",
                    target_path.display()
                ))
            })?;
            continue;
        }

        if file_type.is_symlink() {
            register_path(seen, &rel_path, EntryKind::Symlink, source_name)?;
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    ImageError::BuildFailed(format!(
                        "failed to create parent directory '{}': {error}",
                        parent.display()
                    ))
                })?;
            }
            let link_target = fs::read_link(&path).map_err(|error| {
                ImageError::BuildFailed(format!(
                    "failed to read symlink target '{}' from input '{}': {error}",
                    path.display(),
                    source_name
                ))
            })?;
            create_symlink(&link_target, &target_path)?;
            continue;
        }

        return Err(ImageError::BuildFailed(format!(
            "unsupported filesystem entry '{}' in input '{}'",
            path.display(),
            source_name
        )));
    }

    Ok(())
}

fn register_path(
    seen: &mut HashMap<PathBuf, EntryKind>,
    rel_path: &Path,
    current: EntryKind,
    source_name: &str,
) -> IResult<()> {
    match seen.get(rel_path) {
        None => {
            seen.insert(rel_path.to_path_buf(), current);
            Ok(())
        }
        Some(previous) if *previous == EntryKind::Directory && current == EntryKind::Directory => {
            Ok(())
        }
        Some(previous) => Err(ImageError::BuildFailed(format!(
            "path conflict at '{}' while installing input '{}': already installed as {:?}, new entry is {:?}",
            rel_path.display(),
            source_name,
            previous,
            current
        ))),
    }
}

fn create_rootfs_tar(rootfs_dir: &Path, tar_path: &Path) -> IResult<()> {
    let output = ProcessCommand::new("tar")
        .arg("-C")
        .arg(rootfs_dir)
        .arg("-cf")
        .arg(tar_path)
        .arg(".")
        .output()
        .map_err(|error| ImageError::BuildFailed(format!("failed to execute tar: {error}")))?;

    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "tar failed with exit status {}: {}",
            output.status.code().unwrap_or(1),
            command_details(&output)
        )));
    }

    Ok(())
}

fn podman_import(tar_path: &Path, image_ref: &str) -> IResult<String> {
    let output = ProcessCommand::new("podman")
        .arg("import")
        .arg(tar_path)
        .arg(image_ref)
        .output()
        .map_err(|error| ImageError::BuildFailed(format!("failed to execute podman import: {error}")))?;

    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "podman import failed with exit status {}: {}",
            output.status.code().unwrap_or(1),
            command_details(&output)
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn podman_image_digest(image_ref: &str) -> IResult<String> {
    let output = ProcessCommand::new("podman")
        .arg("image")
        .arg("inspect")
        .arg(image_ref)
        .arg("--format")
        .arg("{{.Digest}}")
        .output()
        .map_err(|error| ImageError::BuildFailed(format!("failed to execute podman image inspect: {error}")))?;

    if !output.status.success() {
        return Err(ImageError::BuildFailed(format!(
            "podman image inspect failed with exit status {}: {}",
            output.status.code().unwrap_or(1),
            command_details(&output)
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn publish_output(
    layout: &WorkspaceLayout,
    output_id: &str,
    binary_inputs: &[&ResolvedInput],
    imported: &ImportedImage,
) -> IResult<()> {
    validate_name(output_id)?;

    let object_path = layout.objects.join(output_id);
    let payload = serde_json::json!({
        "kind": "container-image-ref",
        "image_ref": imported.image_ref,
        "image_id": imported.image_id,
        "image_digest": imported.image_digest,
    });

    fsutil::write_atomic(
        &object_path,
        &serde_json::to_string_pretty(&payload).map_err(|error| {
            ImageError::PublishFailed(format!(
                "failed to serialize image reference payload for '{}': {error}",
                output_id
            ))
        })?,
    )
    .map_err(map_fsutil_error)?;

    let input_names = binary_inputs
        .iter()
        .map(|input| q(&input.name))
        .collect::<Vec<_>>()
        .join(", ");
    let input_ids = binary_inputs
        .iter()
        .map(|input| q(&input.id))
        .collect::<Vec<_>>()
        .join(", ");
    let meta_content = format!(
        "{{\n  id = {},\n  artifact_kind = \"container-image\",\n  producer = {{\n    builder = \"image\",\n    mode = \"bootstrap\",\n  }},\n  attrs = {{\n    image_ref = {},\n    image_id = {},\n    image_digest = {},\n    inputs = [{}],\n    input_ids = [{}],\n  }},\n}}\n",
        q(output_id),
        q(&imported.image_ref),
        q(&imported.image_id),
        q(&imported.image_digest),
        input_names,
        input_ids
    );
    let meta_path = layout.meta.join(format!("{output_id}.ncl"));
    fsutil::write_atomic(&meta_path, &meta_content).map_err(map_fsutil_error)?;

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

fn parse_recipe(value: &Value) -> Result<ImageRecipe, BuilderError> {
    serde_json::from_value::<ImageRecipe>(value.clone())
        .map_err(|error| BuilderError::InvalidRecipe(format!("invalid image recipe: {error}")))
        .and_then(|recipe| {
            validate_recipe(&recipe).map_err(map_error)?;
            Ok(recipe)
        })
}

fn validate_recipe(recipe: &ImageRecipe) -> IResult<()> {
    if recipe.recipe_type != "image" {
        return Err(ImageError::InvalidRecipe(
            "type must be 'image'".to_string(),
        ));
    }

    for input in &recipe.inputs {
        validate_name(input)?;
    }
    for output in &recipe.outputs {
        validate_name(output)?;
    }
    Ok(())
}

fn validate_name(name: &str) -> IResult<()> {
    if name.is_empty() {
        return Err(ImageError::InvalidRecipe(
            "artifact name must not be empty".to_string(),
        ));
    }

    if name == "." || name == ".." {
        return Err(ImageError::InvalidRecipe(format!(
            "invalid artifact name '{name}'"
        )));
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(ImageError::InvalidRecipe(format!(
            "invalid artifact name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }

    Ok(())
}

fn resolve_inputs(layout: &WorkspaceLayout, recipe: &ImageRecipe) -> IResult<Vec<ResolvedInput>> {
    let mut resolved = Vec::with_capacity(recipe.inputs.len());
    for input in &recipe.inputs {
        let ref_path = layout.refs.join(input);
        let object_path = read_ref_target(&ref_path)?;
        let id = object_path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                ImageError::InputResolutionFailed(format!(
                    "failed to derive object id from ref target '{}'",
                    object_path.display()
                ))
            })?
            .to_string();

        let meta_path = layout.meta.join(format!("{id}.ncl"));
        let meta = parse_meta(&meta_path)?;
        if meta.id != id {
            return Err(ImageError::InputResolutionFailed(format!(
                "meta id '{}' does not match ref-resolved object id '{}'",
                meta.id, id
            )));
        }

        resolved.push(ResolvedInput {
            name: input.clone(),
            id,
            object_path,
            artifact_kind: meta.artifact_kind,
        });
    }
    Ok(resolved)
}

fn read_ref_target(ref_path: &Path) -> IResult<PathBuf> {
    if !ref_path.exists() {
        return Err(ImageError::InputResolutionFailed(format!(
            "input ref does not exist: {}",
            ref_path.display()
        )));
    }

    let target = fs::read_link(ref_path).map_err(|error| {
        ImageError::InputResolutionFailed(format!(
            "failed to read ref symlink '{}': {error}",
            ref_path.display()
        ))
    })?;

    let resolved = if target.is_absolute() {
        target
    } else {
        ref_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target)
    };

    if !resolved.exists() {
        return Err(ImageError::InputResolutionFailed(format!(
            "input ref target does not exist: {}",
            resolved.display()
        )));
    }

    Ok(resolved)
}

fn parse_meta(path: &Path) -> IResult<ResolvedMeta> {
    let content = fs::read_to_string(path).map_err(|error| {
        ImageError::InputResolutionFailed(format!(
            "failed to read meta file '{}': {error}",
            path.display()
        ))
    })?;

    let id = extract_ncl_string_field(&content, "id").ok_or_else(|| {
        ImageError::InputResolutionFailed(format!(
            "meta '{}' does not define string field 'id'",
            path.display()
        ))
    })?;
    let artifact_kind = extract_ncl_string_field(&content, "artifact_kind").ok_or_else(|| {
        ImageError::InputResolutionFailed(format!(
            "meta '{}' does not define string field 'artifact_kind'",
            path.display()
        ))
    })?;

    Ok(ResolvedMeta { id, artifact_kind })
}

fn extract_ncl_string_field(content: &str, field: &str) -> Option<String> {
    let key = format!("{field} = \"");
    let start = content.find(&key)? + key.len();
    let rest = &content[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn create_symlink(target: &Path, link_path: &Path) -> IResult<()> {
    #[cfg(unix)]
    {
        unix_fs::symlink(target, link_path).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to create symlink '{}' -> '{}': {error}",
                link_path.display(),
                target.display()
            ))
        })
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        let _ = link_path;
        Err(ImageError::FsFailed(
            "symlink refs are currently supported only on unix hosts".to_string(),
        ))
    }
}

fn replace_symlink(target: &Path, link_path: &Path) -> IResult<()> {
    if link_path.exists() || link_path.is_symlink() {
        let metadata = fs::symlink_metadata(link_path).map_err(|error| {
            ImageError::FsFailed(format!(
                "failed to inspect existing ref '{}': {error}",
                link_path.display()
            ))
        })?;

        if metadata.file_type().is_dir() {
            fs::remove_dir_all(link_path).map_err(|error| {
                ImageError::FsFailed(format!(
                    "failed to remove existing ref directory '{}': {error}",
                    link_path.display()
                ))
            })?;
        } else {
            fs::remove_file(link_path).map_err(|error| {
                ImageError::FsFailed(format!(
                    "failed to remove existing ref '{}': {error}",
                    link_path.display()
                ))
            })?;
        }
    }

    create_symlink(target, link_path)
}

fn recreate_empty_dir(path: &Path) -> IResult<()> {
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path).map_err(|error| {
                ImageError::FsFailed(format!(
                    "failed to remove previous directory '{}': {error}",
                    path.display()
                ))
            })?;
        } else {
            fs::remove_file(path).map_err(|error| {
                ImageError::FsFailed(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

fn map_fsutil_error(error: fsutil::FsUtilError) -> ImageError {
    ImageError::FsFailed(error.to_string())
}

fn workspace_layout() -> IResult<WorkspaceLayout> {
    let cwd = env::current_dir()
        .map_err(|error| ImageError::FsFailed(format!("failed to get current directory: {error}")))?;
    let root = cwd.join(ROOT_DIR);
    Ok(WorkspaceLayout {
        root: root.clone(),
        objects: root.join(OBJECTS_DIR),
        meta: root.join(META_DIR),
        refs: root.join(REFS_DIR),
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> IResult<()> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.objects, "objects")?;
    ensure_dir(&layout.meta, "meta")?;
    ensure_dir(&layout.refs, "refs")?;
    Ok(())
}

fn ensure_dir(path: &Path, label: &str) -> IResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        ImageError::FsFailed(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
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

fn q(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<serialization-error>\"".to_string())
}

fn map_error(error: ImageError) -> BuilderError {
    match error {
        ImageError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        ImageError::InputResolutionFailed(message)
        | ImageError::BuildFailed(message)
        | ImageError::PublishFailed(message)
        | ImageError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}
