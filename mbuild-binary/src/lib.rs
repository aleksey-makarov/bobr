use mbuild_core::{Builder, BuilderError, fsutil};
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

const ROOT_DIR: &str = ".mbuild";
const OBJECTS_DIR: &str = "objects";
const META_DIR: &str = "meta";
const REFS_DIR: &str = "refs";
const STANDARD_IMAGE: &str = "localhost/mbuild-binary:bookworm-toolchain";

const KIND_SOURCE_TREE: &str = "source-tree";
const KIND_BUILD_SCRIPT: &str = "build-script";

type BResult<T> = Result<T, BinaryError>;

#[derive(Debug)]
enum BinaryError {
    InvalidRecipe(String),
    InputResolutionFailed(String),
    PodmanFailed(String),
    BuildFailed(String),
    PublishFailed(String),
    FsFailed(String),
}

#[derive(Debug, Deserialize)]
struct BinaryRecipe {
    #[serde(rename = "type")]
    recipe_type: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
}

#[derive(Debug)]
struct ResolvedInput {
    name: String,
    object_path: PathBuf,
    id: String,
    artifact_kind: String,
}

#[derive(Debug)]
struct ResolvedMeta {
    id: String,
    artifact_kind: String,
}

struct ScriptExecution {
    script_host_path: PathBuf,
    source_input_name: String,
}

pub struct BinaryBuilder;

impl Builder for BinaryBuilder {
    fn get_type(&self) -> &'static str {
        "binary"
    }

    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        let mut ctx = prepare_build_context(artifact, recipe)?;
        prepare_outputs(&mut ctx).map_err(map_error)?;

        let build_result = run_container_build(&ctx);
        if let Err(error) = build_result {
            let _ = cleanup_temp_outputs(&ctx);
            return Err(map_error(error));
        }

        publish_outputs(&ctx).map_err(map_error)?;
        cleanup_temp_outputs(&ctx).map_err(map_error)?;

        println!("build: ok");
        println!("artifact: {}", ctx.artifact_name);
        println!("image: {STANDARD_IMAGE}");
        Ok(())
    }

    fn summarize_recipe(
        &self,
        recipe: &Value,
    ) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let _ = parse_recipe(recipe)?;
        Ok(vec![("script_mode", "input".to_string())])
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
        validate_artifact_name(name)?;
    }
    for name in &recipe.outputs {
        validate_artifact_name(name)?;
    }

    Ok(())
}

fn validate_artifact_name(name: &str) -> BResult<()> {
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

fn run_container_build(ctx: &BuildContext) -> BResult<()> {
    let mut process = ProcessCommand::new("podman");
    process
        .arg("run")
        .arg("--rm")
        .arg("--network=none")
        .arg("--userns=keep-id")
        .arg("--user")
        .arg(format!("{}:{}", ctx.uid, ctx.gid));

    for input in &ctx.inputs {
        if input.artifact_kind == KIND_BUILD_SCRIPT {
            continue;
        }
        process.arg("--volume").arg(format!(
            "{}:/in/{}:O",
            input.object_path.display(),
            input.name
        ));
    }

    for output_name in &ctx.outputs {
        let host_path = ctx.temp_outputs_root.join(output_name);
        process
            .arg("--volume")
            .arg(format!("{}:/out/{}:rw", host_path.display(), output_name));
    }

    let script_mount = format!(
        "{}:/__mbuild_binary_script:ro",
        ctx.script_execution.script_host_path.display()
    );
    let primary_output = ctx.outputs.first().cloned().unwrap_or_default();
    process
        .arg("--volume")
        .arg(script_mount)
        .arg("--env")
        .arg(format!(
            "MBUILD_SOURCE_INPUT={}",
            ctx.script_execution.source_input_name
        ))
        .arg("--env")
        .arg(format!("MBUILD_PRIMARY_OUTPUT={primary_output}"))
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

struct BuildContext {
    artifact_name: String,
    layout: WorkspaceLayout,
    inputs: Vec<ResolvedInput>,
    outputs: Vec<String>,
    temp_outputs_root: PathBuf,
    script_execution: ScriptExecution,
    uid: u32,
    gid: u32,
}

fn prepare_build_context(
    artifact: &str,
    recipe_value: &Value,
) -> Result<BuildContext, BuilderError> {
    let recipe = parse_recipe(recipe_value)?;
    let layout = workspace_layout().map_err(map_error)?;
    ensure_base_dirs(&layout).map_err(map_error)?;

    let inputs = resolve_inputs(&layout, &recipe).map_err(map_error)?;
    let outputs = if recipe.outputs.is_empty() {
        vec![artifact.to_string()]
    } else {
        recipe.outputs.clone()
    };

    let script_execution = resolve_script_execution(&inputs).map_err(map_error)?;

    let timestamp = fsutil::current_epoch_nanos()
        .map_err(map_fsutil_error)
        .map_err(map_error)?;
    let temp_outputs_root = fsutil::temp_root_dir(ROOT_DIR)
        .map_err(map_fsutil_error)
        .map_err(map_error)?
        .join(format!("binary-{}-{}", artifact, timestamp));

    let (uid, gid) = current_uid_gid();

    Ok(BuildContext {
        artifact_name: artifact.to_string(),
        layout,
        inputs,
        outputs,
        temp_outputs_root,
        script_execution,
        uid,
        gid,
    })
}

fn resolve_script_execution(inputs: &[ResolvedInput]) -> BResult<ScriptExecution> {
    let build_scripts: Vec<&ResolvedInput> = inputs
        .iter()
        .filter(|input| input.artifact_kind == KIND_BUILD_SCRIPT)
        .collect();
    let sources: Vec<&ResolvedInput> = inputs
        .iter()
        .filter(|input| input.artifact_kind == KIND_SOURCE_TREE)
        .collect();

    if build_scripts.len() != 1 {
        return Err(BinaryError::InvalidRecipe(format!(
            "binary recipe requires exactly one '{KIND_BUILD_SCRIPT}' input; found {}",
            build_scripts.len()
        )));
    }
    if sources.len() != 1 {
        return Err(BinaryError::InvalidRecipe(format!(
            "binary recipe requires exactly one '{KIND_SOURCE_TREE}' input; found {}",
            sources.len()
        )));
    }

    Ok(ScriptExecution {
        script_host_path: build_scripts[0].object_path.clone(),
        source_input_name: sources[0].name.clone(),
    })
}

fn prepare_outputs(ctx: &mut BuildContext) -> BResult<()> {
    fsutil::recreate_empty_dir(&ctx.temp_outputs_root).map_err(map_fsutil_error)?;
    for output_name in &ctx.outputs {
        fsutil::recreate_empty_dir(&ctx.temp_outputs_root.join(output_name))
            .map_err(map_fsutil_error)?;
    }
    Ok(())
}

fn cleanup_temp_outputs(ctx: &BuildContext) -> BResult<()> {
    if ctx.temp_outputs_root.exists() {
        fsutil::remove_dir_force(&ctx.temp_outputs_root).map_err(map_fsutil_error)?;
    }
    Ok(())
}

fn publish_outputs(ctx: &BuildContext) -> BResult<()> {
    for output_name in &ctx.outputs {
        let tmp_output = ctx.temp_outputs_root.join(output_name);
        if !tmp_output.is_dir() {
            return Err(BinaryError::PublishFailed(format!(
                "declared output '{}' was not created as a directory: {}",
                output_name,
                tmp_output.display()
            )));
        }

        let object_path = ctx.layout.objects.join(output_name);
        replace_dir(&tmp_output, &object_path)?;

        let meta_path = ctx.layout.meta.join(format!("{output_name}.ncl"));
        fsutil::write_atomic(
            &meta_path,
            &render_meta_ncl(output_name, "binary-output", &ctx.inputs, ctx.uid, ctx.gid),
        )
        .map_err(map_fsutil_error)?;

        let ref_path = ctx.layout.refs.join(output_name);
        let ref_target = PathBuf::from("..").join(OBJECTS_DIR).join(output_name);
        replace_symlink(&ref_target, &ref_path)?;

        println!("publish: ok");
        println!("output: {output_name}");
        println!("object: {}", object_path.display());
        println!("meta: {}", meta_path.display());
        println!("ref: {}", ref_path.display());
    }
    Ok(())
}

fn resolve_inputs(layout: &WorkspaceLayout, recipe: &BinaryRecipe) -> BResult<Vec<ResolvedInput>> {
    let mut resolved = Vec::with_capacity(recipe.inputs.len());

    for input in &recipe.inputs {
        let ref_path = layout.refs.join(input);
        let object_path = read_ref_target(&ref_path)?;
        let id = object_path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                BinaryError::InputResolutionFailed(format!(
                    "failed to derive object id from ref target '{}'",
                    object_path.display()
                ))
            })?
            .to_string();

        let meta_path = layout.meta.join(format!("{id}.ncl"));
        let meta = parse_meta(&meta_path)?;

        if meta.id != id {
            return Err(BinaryError::InputResolutionFailed(format!(
                "meta id '{}' does not match ref-resolved object id '{}'",
                meta.id, id
            )));
        }

        if meta.artifact_kind == KIND_BUILD_SCRIPT {
            if !object_path.is_file() {
                return Err(BinaryError::InputResolutionFailed(format!(
                    "build-script input '{}' must resolve to a file: {}",
                    input,
                    object_path.display()
                )));
            }
        } else if !object_path.is_dir() {
            return Err(BinaryError::InputResolutionFailed(format!(
                "input '{}' must resolve to a directory: {}",
                input,
                object_path.display()
            )));
        }

        resolved.push(ResolvedInput {
            name: input.clone(),
            object_path,
            id,
            artifact_kind: meta.artifact_kind,
        });
    }

    Ok(resolved)
}

fn read_ref_target(ref_path: &Path) -> BResult<PathBuf> {
    if !ref_path.exists() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "input ref does not exist: {}",
            ref_path.display()
        )));
    }

    let target = fs::read_link(ref_path).map_err(|error| {
        BinaryError::InputResolutionFailed(format!(
            "failed to read ref symlink '{}': {error}",
            ref_path.display()
        ))
    })?;

    let resolved_target = if target.is_absolute() {
        target
    } else {
        ref_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target)
    };

    if !resolved_target.exists() {
        return Err(BinaryError::InputResolutionFailed(format!(
            "input ref target does not exist: {}",
            resolved_target.display()
        )));
    }

    Ok(resolved_target)
}

fn parse_meta(path: &Path) -> BResult<ResolvedMeta> {
    let content = fs::read_to_string(path).map_err(|error| {
        BinaryError::InputResolutionFailed(format!(
            "failed to read meta file '{}': {error}",
            path.display()
        ))
    })?;

    let id = extract_ncl_string_field(&content, "id").ok_or_else(|| {
        BinaryError::InputResolutionFailed(format!(
            "meta '{}' does not define string field 'id'",
            path.display()
        ))
    })?;
    let artifact_kind = extract_ncl_string_field(&content, "artifact_kind").ok_or_else(|| {
        BinaryError::InputResolutionFailed(format!(
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

fn render_meta_ncl(
    id: &str,
    artifact_kind: &str,
    inputs: &[ResolvedInput],
    owner_uid: u32,
    owner_gid: u32,
) -> String {
    let inputs_list = inputs
        .iter()
        .map(|input| q(&input.name))
        .collect::<Vec<_>>()
        .join(", ");
    let input_ids = inputs
        .iter()
        .map(|input| q(&input.id))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "{{\n  id = {},\n  artifact_kind = {},\n  producer = {{\n    builder = \"binary\",\n  }},\n  attrs = {{\n    inputs = [{}],\n    input_ids = [{}],\n    install = {{\n      owners = [\n        {{ path = \"**\", uid = {}, gid = {} }},\n      ],\n    }},\n  }},\n}}\n",
        q(id),
        q(artifact_kind),
        inputs_list,
        input_ids,
        owner_uid,
        owner_gid
    )
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

fn replace_dir(tmp_dir: &Path, destination: &Path) -> BResult<()> {
    if destination.exists() {
        if destination.is_dir() {
            fs::remove_dir_all(destination).map_err(|error| {
                BinaryError::PublishFailed(format!(
                    "failed to remove previous object directory '{}': {error}",
                    destination.display()
                ))
            })?;
        } else {
            fs::remove_file(destination).map_err(|error| {
                BinaryError::PublishFailed(format!(
                    "failed to remove previous object file '{}': {error}",
                    destination.display()
                ))
            })?;
        }
    }

    fs::rename(tmp_dir, destination).map_err(|error| {
        BinaryError::PublishFailed(format!(
            "failed to publish output '{}' -> '{}': {error}",
            tmp_dir.display(),
            destination.display()
        ))
    })
}

fn replace_symlink(target: &Path, link_path: &Path) -> BResult<()> {
    if link_path.exists() || link_path.is_symlink() {
        let metadata = fs::symlink_metadata(link_path).map_err(|error| {
            BinaryError::FsFailed(format!(
                "failed to inspect existing ref '{}': {error}",
                link_path.display()
            ))
        })?;

        if metadata.file_type().is_dir() {
            fs::remove_dir_all(link_path).map_err(|error| {
                BinaryError::FsFailed(format!(
                    "failed to remove existing ref directory '{}': {error}",
                    link_path.display()
                ))
            })?;
        } else {
            fs::remove_file(link_path).map_err(|error| {
                BinaryError::FsFailed(format!(
                    "failed to remove existing ref '{}': {error}",
                    link_path.display()
                ))
            })?;
        }
    }

    create_symlink(target, link_path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> BResult<()> {
    unix_fs::symlink(target, link_path).map_err(|error| {
        BinaryError::FsFailed(format!(
            "failed to create ref symlink '{}' -> '{}': {error}",
            link_path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link_path: &Path) -> BResult<()> {
    Err(BinaryError::FsFailed(
        "symlink refs are currently supported only on unix hosts".to_string(),
    ))
}

fn q(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<serialization-error>\"".to_string())
}

fn current_uid_gid() -> (u32, u32) {
    #[cfg(unix)]
    {
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
    objects: PathBuf,
    meta: PathBuf,
    refs: PathBuf,
}

fn workspace_layout() -> BResult<WorkspaceLayout> {
    let cwd = env::current_dir()
        .map_err(|error| BinaryError::FsFailed(format!("failed to get current directory: {error}")))?;
    let root = cwd.join(ROOT_DIR);

    Ok(WorkspaceLayout {
        root: root.clone(),
        objects: root.join(OBJECTS_DIR),
        meta: root.join(META_DIR),
        refs: root.join(REFS_DIR),
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> BResult<()> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.objects, "objects")?;
    ensure_dir(&layout.meta, "meta")?;
    ensure_dir(&layout.refs, "refs")?;
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

fn map_fsutil_error(error: fsutil::FsUtilError) -> BinaryError {
    BinaryError::FsFailed(error.to_string())
}

fn map_error(error: BinaryError) -> BuilderError {
    match error {
        BinaryError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        BinaryError::InputResolutionFailed(message)
        | BinaryError::PodmanFailed(message)
        | BinaryError::BuildFailed(message)
        | BinaryError::PublishFailed(message)
        | BinaryError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}
