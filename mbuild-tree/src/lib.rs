use fsobj_hash::{ObjectHash, hash_path};
use globset::{Glob, GlobMatcher};
use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    ComposedFsTree, ComposedFsTreeEntry, FsTreeComposeInput, FsTreeEntry, FsTreeManifest,
    FsTreeOwnerMap, StagedBuildResult, TypedBuilder, compose_fs_trees, create_fs_tree_staging_dir,
    fsutil,
};
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::collections::BTreeMap;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    tree: TreePayload,
    #[serde(default)]
    install: Option<InstallMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TreePayload {
    entries: Vec<TreeEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TreeEntry {
    File {
        path: String,
        text: String,
        executable: bool,
    },
    Dir {
        path: String,
    },
    Symlink {
        path: String,
        target: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallMeta {
    rules: Vec<InstallRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallRule {
    path: String,
    attrs: InstallAttrs,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallAttrs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    regular_file_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executable_file_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symlink_mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormalizedEntry {
    File {
        rel_path: String,
        text: String,
        executable: bool,
    },
    Dir {
        rel_path: String,
    },
    Symlink {
        rel_path: String,
        target: String,
    },
}

impl NormalizedEntry {
    fn rel_path(&self) -> &str {
        match self {
            Self::File { rel_path, .. }
            | Self::Dir { rel_path }
            | Self::Symlink { rel_path, .. } => rel_path,
        }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::Dir { .. } => "directory",
            Self::Symlink { .. } => "symlink",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    File,
    Directory,
}

#[derive(Debug)]
struct CompiledInstallRule {
    pattern: String,
    matcher: GlobMatcher,
    attrs: InstallAttrs,
}

#[derive(Debug, Clone)]
enum MaterializedKind {
    File { executable: bool },
    Directory,
    Symlink { target: String },
}

trait OwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        object_dir: &Path,
        manifest: &FsTreeManifest,
        temp_dir: &Path,
    ) -> Result<(), BuilderError>;

    fn materialize_selected_and_hash(
        &self,
        root_dir: &Path,
        object_dir: &Path,
        materialize_manifest: &FsTreeManifest,
        _object_manifest: &FsTreeManifest,
        temp_dir: &Path,
    ) -> Result<ObjectHash, BuilderError> {
        self.materialize_and_validate(root_dir, object_dir, materialize_manifest, temp_dir)?;
        hash_path(object_dir).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to hash staged tree object '{}': {error}",
                object_dir.display()
            ))
        })
    }

    fn validate_hardlinked_file(
        &self,
        source: &Path,
        manifest_entry: &FsTreeEntry,
    ) -> Result<(), BuilderError>;
}

#[derive(Debug, Clone, Copy)]
struct RuntimeOwnershipMaterializer;

impl OwnershipMaterializer for RuntimeOwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        _object_dir: &Path,
        manifest: &FsTreeManifest,
        temp_dir: &Path,
    ) -> Result<(), BuilderError> {
        let idmap = mbuild_runtime::cached_host_idmap()
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        mbuild_runtime::apply_ownership_batch(root_dir, manifest, idmap.as_ref(), temp_dir)
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        Ok(())
    }

    fn materialize_selected_and_hash(
        &self,
        root_dir: &Path,
        _object_dir: &Path,
        materialize_manifest: &FsTreeManifest,
        object_manifest: &FsTreeManifest,
        temp_dir: &Path,
    ) -> Result<ObjectHash, BuilderError> {
        let idmap = mbuild_runtime::cached_host_idmap()
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        mbuild_runtime::apply_selected_ownership_batch_and_hash_fs_tree_object(
            root_dir,
            materialize_manifest,
            object_manifest,
            idmap.as_ref(),
            temp_dir,
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
    }

    fn validate_hardlinked_file(
        &self,
        source: &Path,
        manifest_entry: &FsTreeEntry,
    ) -> Result<(), BuilderError> {
        let idmap = mbuild_runtime::cached_host_idmap()
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        validate_tree_merge_file_attrs(source, manifest_entry, idmap.as_ref())
    }
}

pub struct TreeBuilder;
pub struct TreeMergeBuilder;

static TREE_SPEC: BuilderSpec = BuilderSpec {
    tag: "Tree",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeMergeConfig {}

static TREE_MERGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "TreeMerge",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for TreeBuilder {
    type Config = TreeConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &TREE_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree(config, inputs, cx, &RuntimeOwnershipMaterializer)
    }
}

impl TypedBuilder for TreeMergeBuilder {
    type Config = TreeMergeConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &TREE_MERGE_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree_merge(config, inputs, cx, &RuntimeOwnershipMaterializer)
    }
}

fn build_tree(
    config: TreeConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    materializer: &impl OwnershipMaterializer,
) -> Result<StagedBuildResult, BuilderError> {
    if !inputs.is_empty() {
        return Err(BuilderError::ExecutionFailed(
            "Tree builder does not accept input objects".to_string(),
        ));
    }

    let normalized = normalize_entries(config.tree.entries)?;
    let output_kind = determine_output_kind(&normalized);
    validate_install(output_kind, config.install.as_ref())?;

    let now_nanos = fsutil::current_epoch_nanos()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let output_path = cx.temp_dir.join(format!("tree-{now_nanos}.obj"));

    cx.log_event(
        BuildLogLevel::Info,
        "stage",
        format!("materializing tree output '{}'", output_path.display()),
    );

    let (meta, object_hash) = match output_kind {
        OutputKind::File => {
            materialize_file_output(&output_path, &normalized)?;
            (Map::new(), None)
        }
        OutputKind::Directory => {
            let install = config
                .install
                .expect("validated install for directory output");
            let object_hash = materialize_directory_output(
                &output_path,
                &normalized,
                &install,
                &cx.temp_dir,
                materializer,
            )?;
            (Map::new(), Some(object_hash))
        }
    };

    Ok(StagedBuildResult {
        meta,
        staged_path: output_path,
        object_hash,
    })
}

fn build_tree_merge(
    _config: TreeMergeConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    materializer: &impl OwnershipMaterializer,
) -> Result<StagedBuildResult, BuilderError> {
    let inputs = inputs.extras(&TREE_MERGE_SPEC).collect::<Vec<_>>();
    if inputs.len() < 2 {
        return Err(BuilderError::ExecutionFailed(
            "TreeMerge builder requires at least two fs-tree inputs".to_string(),
        ));
    }

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!("merging {} fs-tree input(s)", inputs.len()),
    );

    let compose_inputs = inputs
        .iter()
        .map(|(name, object)| tree_merge_input(name, object))
        .collect::<Result<Vec<_>, _>>()?;
    let composed = compose_fs_trees(&compose_inputs).map_err(map_fs_tree_error)?;

    let now_nanos = fsutil::current_epoch_nanos()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let output_path = cx.temp_dir.join(format!("tree-merge-{now_nanos}.obj"));

    cx.log_event(
        BuildLogLevel::Info,
        "materialize",
        format!("materializing merged fs-tree '{}'", output_path.display()),
    );

    let object_hash =
        materialize_tree_merge_output(&output_path, &composed, &cx.temp_dir, materializer)?;

    Ok(StagedBuildResult {
        meta: Map::new(),
        staged_path: output_path,
        object_hash: Some(object_hash),
    })
}

fn tree_merge_input(
    name: &str,
    object: &BuilderInputObject,
) -> Result<FsTreeComposeInput, BuilderError> {
    let input = load_fs_tree_compose_input(&object.object_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "TreeMerge input '{name}' is not a valid fs-tree object: {error}"
        ))
    })?;
    Ok(input)
}

fn load_fs_tree_compose_input(object_path: &Path) -> Result<FsTreeComposeInput, BuilderError> {
    let manifest_path = object_path.join("manifest.jsonl");
    let root_dir = object_path.join("root");
    require_directory(object_path, "fs-tree object directory")?;
    require_regular_non_executable_file(&manifest_path, "fs-tree manifest")?;
    require_directory(&root_dir, "fs-tree root directory")?;
    let manifest = FsTreeManifest::read_canonical(&manifest_path)
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    Ok(FsTreeComposeInput { manifest, root_dir })
}

fn require_directory(path: &Path, label: &str) -> Result<(), BuilderError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(BuilderError::ExecutionFailed(format!(
            "{label} '{}' must be a directory",
            path.display()
        )))
    }
}

fn require_regular_non_executable_file(path: &Path, label: &str) -> Result<(), BuilderError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(BuilderError::ExecutionFailed(format!(
            "{label} '{}' must be a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 != 0 {
        return Err(BuilderError::ExecutionFailed(format!(
            "{label} '{}' must not be executable",
            path.display()
        )));
    }
    Ok(())
}

fn materialize_tree_merge_output(
    output_path: &Path,
    composed: &ComposedFsTree,
    temp_dir: &Path,
    materializer: &impl OwnershipMaterializer,
) -> Result<ObjectHash, BuilderError> {
    let paths =
        create_fs_tree_staging_dir(output_path, composed.manifest()).map_err(map_fs_tree_error)?;
    let mut materialize_entries = Vec::new();

    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(composed.entries())
    {
        match (manifest_entry, composed_entry) {
            (FsTreeEntry::Directory { path, .. }, ComposedFsTreeEntry::Directory) => {
                if !path.is_empty() {
                    fs::create_dir(paths.root_dir.join(path)).map_err(|error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to create merged fs-tree directory '{}': {error}",
                            paths.root_dir.join(path).display()
                        ))
                    })?;
                }
                materialize_entries.push(manifest_entry.clone());
            }
            (FsTreeEntry::File { path, .. }, ComposedFsTreeEntry::File { source_path }) => {
                let dst = paths.root_dir.join(path);
                if link_or_copy_tree_merge_file(source_path, &dst, manifest_entry, materializer)?
                    == TreeMergeFileMaterialization::Copied
                {
                    materialize_entries.push(manifest_entry.clone());
                }
            }
            (
                FsTreeEntry::Symlink { path, target, .. },
                ComposedFsTreeEntry::Symlink { source_path },
            ) => {
                let dst = paths.root_dir.join(path);
                create_tree_merge_symlink(source_path, &dst, target)?;
                materialize_entries.push(manifest_entry.clone());
            }
            _ => {
                return Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                )));
            }
        }
    }

    let materialize_manifest =
        FsTreeManifest::from_entries(materialize_entries).map_err(map_fs_tree_error)?;
    materializer.materialize_selected_and_hash(
        &paths.root_dir,
        &paths.object_dir,
        &materialize_manifest,
        composed.manifest(),
        temp_dir,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeMergeFileMaterialization {
    Hardlinked,
    Copied,
}

fn link_or_copy_tree_merge_file(
    source: &Path,
    dst: &Path,
    manifest_entry: &FsTreeEntry,
    materializer: &impl OwnershipMaterializer,
) -> Result<TreeMergeFileMaterialization, BuilderError> {
    materializer.validate_hardlinked_file(source, manifest_entry)?;
    match fs::hard_link(source, dst) {
        Ok(()) => Ok(TreeMergeFileMaterialization::Hardlinked),
        Err(error) if should_copy_after_link_error(error.kind()) => {
            fs::copy(source, dst).map_err(|copy_error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to copy merged fs-tree file '{}' to '{}': {copy_error}",
                    source.display(),
                    dst.display()
                ))
            })?;
            Ok(TreeMergeFileMaterialization::Copied)
        }
        Err(error) => Err(BuilderError::ExecutionFailed(format!(
            "failed to hardlink merged fs-tree file '{}' to '{}': {error}",
            source.display(),
            dst.display()
        ))),
    }
}

fn should_copy_after_link_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::CrossesDevices
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
            | io::ErrorKind::TooManyLinks
    )
}

#[cfg(unix)]
fn validate_tree_merge_file_attrs(
    source: &Path,
    manifest_entry: &FsTreeEntry,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), BuilderError> {
    let FsTreeEntry::File {
        path,
        uid,
        gid,
        mode,
    } = manifest_entry
    else {
        return Err(BuilderError::ExecutionFailed(format!(
            "expected file manifest entry for '{}'",
            source.display()
        )));
    };

    let metadata = fs::symlink_metadata(source).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to inspect merged fs-tree source file '{}': {error}",
            source.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source '{}' for '{}' must be a file",
            source.display(),
            path
        )));
    }

    let expected_uid = owner_map.physical_uid(*uid).map_err(map_fs_tree_error)?;
    let expected_gid = owner_map.physical_gid(*gid).map_err(map_fs_tree_error)?;
    if metadata.uid() != expected_uid || metadata.gid() != expected_gid {
        return Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source file '{}' for '{}' has owner {}:{}, expected {}:{}",
            source.display(),
            path,
            metadata.uid(),
            metadata.gid(),
            expected_uid,
            expected_gid
        )));
    }

    let actual_mode = metadata.permissions().mode() & 0o7777;
    if actual_mode != *mode {
        return Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source file '{}' for '{}' has mode {:o}, expected {:o}",
            source.display(),
            path,
            actual_mode,
            mode
        )));
    }

    Ok(())
}

#[cfg(not(unix))]
fn validate_tree_merge_file_attrs(
    source: &Path,
    manifest_entry: &FsTreeEntry,
    _owner_map: &impl FsTreeOwnerMap,
) -> Result<(), BuilderError> {
    if source.is_file() {
        Ok(())
    } else {
        Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source '{}' for '{}' must be a file",
            source.display(),
            manifest_entry.path()
        )))
    }
}

#[cfg(unix)]
fn create_tree_merge_symlink(
    source: &Path,
    dst: &Path,
    expected: &str,
) -> Result<(), BuilderError> {
    let target = fs::read_link(source).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to read merged fs-tree source symlink '{}': {error}",
            source.display()
        ))
    })?;
    let actual = target.to_str().ok_or_else(|| {
        BuilderError::ExecutionFailed(format!(
            "merged fs-tree source symlink '{}' has non-UTF-8 target",
            source.display()
        ))
    })?;
    if actual != expected {
        return Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source symlink '{}' has target '{}', expected '{}'",
            source.display(),
            actual,
            expected
        )));
    }
    symlink(&target, dst).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create merged fs-tree symlink '{}': {error}",
            dst.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_tree_merge_symlink(
    _source: &Path,
    _dst: &Path,
    _expected: &str,
) -> Result<(), BuilderError> {
    Err(BuilderError::ExecutionFailed(
        "TreeMerge symlink materialization is only supported on unix platforms".to_string(),
    ))
}

fn normalize_entries(entries: Vec<TreeEntry>) -> Result<Vec<NormalizedEntry>, BuilderError> {
    if entries.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: tree.entries must not be empty".to_string(),
        ));
    }

    let mut normalized = entries
        .into_iter()
        .map(|entry| match entry {
            TreeEntry::File {
                path,
                text,
                executable,
            } => Ok(NormalizedEntry::File {
                rel_path: validate_rel_path(&path)?,
                text,
                executable,
            }),
            TreeEntry::Dir { path } => Ok(NormalizedEntry::Dir {
                rel_path: validate_rel_path(&path)?,
            }),
            TreeEntry::Symlink { path, target } => {
                if target.is_empty() {
                    return Err(BuilderError::InvalidRecipe(
                        "invalid builder config: symlink target must not be empty".to_string(),
                    ));
                }
                Ok(NormalizedEntry::Symlink {
                    rel_path: validate_rel_path(&path)?,
                    target,
                })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    normalized.sort_by(|left, right| left.rel_path().cmp(right.rel_path()));

    let mut kinds_by_path = BTreeMap::new();
    for entry in &normalized {
        let rel_path = entry.rel_path().to_string();
        if kinds_by_path
            .insert(rel_path.clone(), entry.kind_name())
            .is_some()
        {
            return Err(BuilderError::InvalidRecipe(format!(
                "invalid builder config: duplicate tree entry path '{rel_path}'"
            )));
        }
    }

    for entry in &normalized {
        let rel_path = entry.rel_path();
        let mut current = PathBuf::new();
        let components = rel_path.split('/').collect::<Vec<_>>();
        for segment in components.iter().take(components.len().saturating_sub(1)) {
            current.push(segment);
            let ancestor = current.to_string_lossy();
            if let Some(kind) = kinds_by_path.get(ancestor.as_ref())
                && matches!(*kind, "file" | "symlink")
            {
                return Err(BuilderError::InvalidRecipe(format!(
                    "invalid builder config: {} entry '{}' conflicts with descendant path '{}'",
                    kind, ancestor, rel_path
                )));
            }
        }
    }

    Ok(normalized)
}

fn determine_output_kind(entries: &[NormalizedEntry]) -> OutputKind {
    if entries.len() == 1 {
        if let Some(NormalizedEntry::File { rel_path, .. }) = entries.first()
            && !rel_path.contains('/')
        {
            return OutputKind::File;
        }
    }
    OutputKind::Directory
}

fn validate_install(kind: OutputKind, install: Option<&InstallMeta>) -> Result<(), BuilderError> {
    match (kind, install) {
        (OutputKind::File, Some(_)) => Err(BuilderError::InvalidRecipe(
            "invalid builder config: file output must not specify install".to_string(),
        )),
        (OutputKind::File, None) => Ok(()),
        (OutputKind::Directory, None) => Err(BuilderError::InvalidRecipe(
            "invalid builder config: directory output requires install".to_string(),
        )),
        (OutputKind::Directory, Some(install)) => {
            if install.rules.is_empty() {
                return Err(BuilderError::InvalidRecipe(
                    "invalid builder config: install.rules must contain at least one rule"
                        .to_string(),
                ));
            }
            Ok(())
        }
    }
}

fn validate_rel_path(path: &str) -> Result<String, BuilderError> {
    if path.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: tree entry path must not be empty".to_string(),
        ));
    }
    let path_obj = Path::new(path);
    if path_obj.is_absolute() {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: tree entry path '{path}' must be relative"
        )));
    }
    if path.split('/').any(str::is_empty) {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: tree entry path '{path}' must not contain empty segments"
        )));
    }
    if path.contains('\\') {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: tree entry path '{path}' must use '/' separators"
        )));
    }

    let mut normalized = Vec::new();
    for component in path_obj.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment.to_string_lossy().to_string()),
            Component::CurDir => {
                return Err(BuilderError::InvalidRecipe(format!(
                    "invalid builder config: tree entry path '{path}' must not contain '.'"
                )));
            }
            Component::ParentDir => {
                return Err(BuilderError::InvalidRecipe(format!(
                    "invalid builder config: tree entry path '{path}' must not contain '..'"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(BuilderError::InvalidRecipe(format!(
                    "invalid builder config: tree entry path '{path}' must be relative"
                )));
            }
        }
    }

    if normalized.is_empty() {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: tree entry path '{path}' must not be empty"
        )));
    }

    Ok(normalized.join("/"))
}

fn materialize_file_output(path: &Path, entries: &[NormalizedEntry]) -> Result<(), BuilderError> {
    let NormalizedEntry::File {
        text, executable, ..
    } = entries.first().expect("validated non-empty tree entries")
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: file output requires one file entry".to_string(),
        ));
    };

    if path.exists() {
        fs::remove_file(path).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to remove previous file '{}': {error}",
                path.display()
            ))
        })?;
    }

    fs::write(path, text).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to write staged file '{}': {error}",
            path.display()
        ))
    })?;
    set_file_mode(path, *executable)?;
    Ok(())
}

fn materialize_directory_output(
    object_dir: &Path,
    entries: &[NormalizedEntry],
    install: &InstallMeta,
    temp_dir: &Path,
    materializer: &impl OwnershipMaterializer,
) -> Result<ObjectHash, BuilderError> {
    let rules = compile_install_rules(&install.rules)?;
    let manifest = fs_tree_manifest_for_entries(entries, &rules)?;
    let paths = create_fs_tree_staging_dir(object_dir, &manifest).map_err(map_fs_tree_error)?;

    for manifest_entry in manifest.entries() {
        if let FsTreeEntry::Directory { path, .. } = manifest_entry {
            if path.is_empty() {
                continue;
            }
            let dst = paths.root_dir.join(path);
            fs::create_dir(&dst).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to create staged directory '{}': {error}",
                    dst.display()
                ))
            })?;
        }
    }

    for entry in entries {
        match entry {
            NormalizedEntry::Dir { .. } => {}
            NormalizedEntry::File { rel_path, text, .. } => {
                let path = paths.root_dir.join(rel_path);
                fs::write(&path, text).map_err(|error| {
                    BuilderError::ExecutionFailed(format!(
                        "failed to write staged file '{}': {error}",
                        path.display()
                    ))
                })?;
                if let Some(FsTreeEntry::File { mode, .. }) = manifest
                    .entries()
                    .iter()
                    .find(|entry| entry.path() == rel_path)
                {
                    set_mode(&path, *mode)?;
                }
            }
            NormalizedEntry::Symlink { rel_path, target } => {
                let path = paths.root_dir.join(rel_path);
                create_symlink(target, &path)?;
            }
        }
    }

    apply_directory_modes_post_order(&manifest, &paths.root_dir)?;
    let object_hash = hash_path(object_dir).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to hash staged tree object '{}': {error}",
            object_dir.display()
        ))
    })?;
    materializer.materialize_and_validate(&paths.root_dir, object_dir, &manifest, temp_dir)?;
    Ok(object_hash)
}

fn apply_directory_modes_post_order(
    manifest: &FsTreeManifest,
    root_dir: &Path,
) -> Result<(), BuilderError> {
    let mut dirs = manifest
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            FsTreeEntry::Directory { path, mode, .. } if !path.is_empty() => {
                Some((path.as_str(), *mode))
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    dirs.sort_by(|(left, _), (right, _)| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });

    for (path, mode) in dirs {
        set_mode(&root_dir.join(path), mode)?;
    }

    Ok(())
}

fn fs_tree_manifest_for_entries(
    entries: &[NormalizedEntry],
    rules: &[CompiledInstallRule],
) -> Result<FsTreeManifest, BuilderError> {
    let mut manifest_entries = BTreeMap::<String, FsTreeEntry>::new();
    manifest_entries.insert(String::new(), FsTreeEntry::directory("", 0, 0, 0o755));

    for entry in entries {
        add_parent_directories(entry.rel_path(), &mut manifest_entries, rules)?;
        let fs_entry = match entry {
            NormalizedEntry::File {
                rel_path,
                executable,
                ..
            } => fs_tree_entry_for_path(
                rel_path,
                MaterializedKind::File {
                    executable: *executable,
                },
                rules,
            )?,
            NormalizedEntry::Dir { rel_path } => {
                fs_tree_entry_for_path(rel_path, MaterializedKind::Directory, rules)?
            }
            NormalizedEntry::Symlink { rel_path, target } => fs_tree_entry_for_path(
                rel_path,
                MaterializedKind::Symlink {
                    target: target.clone(),
                },
                rules,
            )?,
        };
        manifest_entries.insert(entry.rel_path().to_string(), fs_entry);
    }

    FsTreeManifest::from_entries(manifest_entries.into_values().collect()).map_err(|error| {
        BuilderError::InvalidRecipe(format!("invalid builder config: fs-tree manifest: {error}"))
    })
}

fn add_parent_directories(
    rel_path: &str,
    manifest_entries: &mut BTreeMap<String, FsTreeEntry>,
    rules: &[CompiledInstallRule],
) -> Result<(), BuilderError> {
    let mut current = PathBuf::new();
    let components = rel_path.split('/').collect::<Vec<_>>();
    for segment in components.iter().take(components.len().saturating_sub(1)) {
        current.push(segment);
        let path = current.to_string_lossy().replace('\\', "/");
        if !manifest_entries.contains_key(&path) {
            let entry = fs_tree_entry_for_path(&path, MaterializedKind::Directory, rules)?;
            manifest_entries.insert(path, entry);
        }
    }
    Ok(())
}

fn fs_tree_entry_for_path(
    rel_path: &str,
    kind: MaterializedKind,
    rules: &[CompiledInstallRule],
) -> Result<FsTreeEntry, BuilderError> {
    let attrs = resolve_install_attrs(rel_path, rules)?;
    let uid = required_attr(attrs.uid, rel_path, "uid")?;
    let gid = required_attr(attrs.gid, rel_path, "gid")?;
    match kind {
        MaterializedKind::Directory => Ok(FsTreeEntry::directory(
            rel_path,
            uid,
            gid,
            required_attr(attrs.directory_mode, rel_path, "directory_mode")?,
        )),
        MaterializedKind::File { executable } => {
            let mode = if executable {
                required_attr(attrs.executable_file_mode, rel_path, "executable_file_mode")?
            } else {
                required_attr(attrs.regular_file_mode, rel_path, "regular_file_mode")?
            };
            Ok(FsTreeEntry::file(rel_path, uid, gid, mode))
        }
        MaterializedKind::Symlink { target } => {
            Ok(FsTreeEntry::symlink(rel_path, uid, gid, target))
        }
    }
}

fn required_attr(value: Option<u32>, rel_path: &str, name: &str) -> Result<u32, BuilderError> {
    value.ok_or_else(|| {
        BuilderError::InvalidRecipe(format!(
            "invalid builder config: path '{rel_path}' is missing resolved {name}"
        ))
    })
}

fn compile_install_rules(rules: &[InstallRule]) -> Result<Vec<CompiledInstallRule>, BuilderError> {
    rules
        .iter()
        .map(|rule| {
            let glob = Glob::new(&rule.path).map_err(|error| {
                BuilderError::InvalidRecipe(format!(
                    "invalid builder config: invalid install rule pattern '{}': {error}",
                    rule.path
                ))
            })?;
            Ok(CompiledInstallRule {
                pattern: rule.path.clone(),
                matcher: glob.compile_matcher(),
                attrs: rule.attrs.clone(),
            })
        })
        .collect()
}

fn resolve_install_attrs(
    rel_path: &str,
    rules: &[CompiledInstallRule],
) -> Result<InstallAttrs, BuilderError> {
    let mut resolved = InstallAttrs::default();
    let mut matched_any = false;
    for rule in rules {
        if install_rule_matches(rule, rel_path) {
            matched_any = true;
            if let Some(uid) = rule.attrs.uid {
                resolved.uid = Some(uid);
            }
            if let Some(gid) = rule.attrs.gid {
                resolved.gid = Some(gid);
            }
            if let Some(mode) = rule.attrs.directory_mode {
                resolved.directory_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.regular_file_mode {
                resolved.regular_file_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.executable_file_mode {
                resolved.executable_file_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.symlink_mode {
                resolved.symlink_mode = Some(mode);
            }
        }
    }

    if matched_any {
        Ok(resolved)
    } else {
        let known = rules
            .iter()
            .map(|rule| rule.pattern.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: path '{rel_path}' is not covered by any install rule (known patterns: {known})"
        )))
    }
}

fn install_rule_matches(rule: &CompiledInstallRule, rel_path: &str) -> bool {
    if rule.matcher.is_match(rel_path) {
        return true;
    }

    if let Some(prefix) = rule.pattern.strip_suffix("/**") {
        return rel_path == prefix;
    }

    false
}

fn set_file_mode(path: &Path, executable: bool) -> Result<(), BuilderError> {
    #[cfg(unix)]
    {
        let mode = if executable { 0o755 } else { 0o644 };
        set_mode(path, mode)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), BuilderError> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to set permissions on staged path '{}': {error}",
                path.display()
            ))
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = mode;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> Result<(), BuilderError> {
    std::os::unix::fs::symlink(target, path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create staged symlink '{}' -> '{}': {error}",
            path.display(),
            target
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, _path: &Path) -> Result<(), BuilderError> {
    Err(BuilderError::ExecutionFailed(
        "Tree symlink entries are only supported on unix platforms".to_string(),
    ))
}

fn map_fs_tree_error(error: impl std::fmt::Display) -> BuilderError {
    BuilderError::ExecutionFailed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{
        Builder, BuilderInputObject, BuilderInputs, FsTreeObjectError, FsTreeOwnerMap,
        validate_fs_tree_object,
    };
    use std::cell::RefCell;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    #[derive(Debug, Clone, Copy)]
    struct CurrentOwnerMaterializer;

    #[derive(Debug, Clone, Copy)]
    struct FixedHashMaterializer;

    #[derive(Debug)]
    struct RecordingMaterializer {
        materialized_paths: RefCell<Vec<String>>,
    }

    impl OwnershipMaterializer for CurrentOwnerMaterializer {
        fn materialize_and_validate(
            &self,
            root_dir: &Path,
            object_dir: &Path,
            manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<(), BuilderError> {
            let owner_map = current_owner_map(root_dir)?;
            for entry in manifest.entries() {
                let (uid, gid) = match entry {
                    FsTreeEntry::File { uid, gid, .. }
                    | FsTreeEntry::Directory { uid, gid, .. }
                    | FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid),
                };
                if uid != 0 || gid != 0 {
                    return Err(BuilderError::ExecutionFailed(format!(
                        "test materializer supports only logical uid=0,gid=0, got uid={uid},gid={gid} for '{}'",
                        entry.path()
                    )));
                }
            }
            validate_fs_tree_object(object_dir, &owner_map).map_err(map_fs_tree_error)?;
            Ok(())
        }

        fn materialize_selected_and_hash(
            &self,
            root_dir: &Path,
            object_dir: &Path,
            materialize_manifest: &FsTreeManifest,
            _object_manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<ObjectHash, BuilderError> {
            apply_test_modes(materialize_manifest, root_dir)?;
            let owner_map = current_owner_map(root_dir)?;
            validate_fs_tree_object(object_dir, &owner_map).map_err(map_fs_tree_error)?;
            hash_path(object_dir).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to hash staged tree object '{}': {error}",
                    object_dir.display()
                ))
            })
        }

        fn validate_hardlinked_file(
            &self,
            source: &Path,
            manifest_entry: &FsTreeEntry,
        ) -> Result<(), BuilderError> {
            let owner_map = current_owner_map(source)?;
            validate_tree_merge_file_attrs(source, manifest_entry, &owner_map)
        }
    }

    impl OwnershipMaterializer for FixedHashMaterializer {
        fn materialize_and_validate(
            &self,
            _root_dir: &Path,
            _object_dir: &Path,
            _manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<(), BuilderError> {
            Ok(())
        }

        fn materialize_selected_and_hash(
            &self,
            root_dir: &Path,
            _object_dir: &Path,
            materialize_manifest: &FsTreeManifest,
            _object_manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<ObjectHash, BuilderError> {
            apply_test_modes(materialize_manifest, root_dir)?;
            Ok(fixed_object_hash())
        }

        fn validate_hardlinked_file(
            &self,
            _source: &Path,
            _manifest_entry: &FsTreeEntry,
        ) -> Result<(), BuilderError> {
            Ok(())
        }
    }

    impl OwnershipMaterializer for RecordingMaterializer {
        fn materialize_and_validate(
            &self,
            _root_dir: &Path,
            _object_dir: &Path,
            _manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<(), BuilderError> {
            Ok(())
        }

        fn materialize_selected_and_hash(
            &self,
            _root_dir: &Path,
            _object_dir: &Path,
            materialize_manifest: &FsTreeManifest,
            _object_manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<ObjectHash, BuilderError> {
            self.materialized_paths.replace(
                materialize_manifest
                    .entries()
                    .iter()
                    .map(|entry| entry.path().to_string())
                    .collect(),
            );
            Ok(fixed_object_hash())
        }

        fn validate_hardlinked_file(
            &self,
            source: &Path,
            manifest_entry: &FsTreeEntry,
        ) -> Result<(), BuilderError> {
            let owner_map = current_owner_map(source)?;
            validate_tree_merge_file_attrs(source, manifest_entry, &owner_map)
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct CurrentOwnerMap {
        uid: u32,
        gid: u32,
    }

    impl FsTreeOwnerMap for CurrentOwnerMap {
        fn physical_uid(&self, logical_uid: u32) -> Result<u32, FsTreeObjectError> {
            if logical_uid == 0 {
                Ok(self.uid)
            } else {
                Err(FsTreeObjectError::Invalid(format!(
                    "test owner map supports only logical uid 0, got {logical_uid}"
                )))
            }
        }

        fn physical_gid(&self, logical_gid: u32) -> Result<u32, FsTreeObjectError> {
            if logical_gid == 0 {
                Ok(self.gid)
            } else {
                Err(FsTreeObjectError::Invalid(format!(
                    "test owner map supports only logical gid 0, got {logical_gid}"
                )))
            }
        }
    }

    impl TreeBuilder {
        fn build_typed_for_tests(
            &self,
            config: TreeConfig,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            build_tree(config, inputs, cx, &CurrentOwnerMaterializer)
        }
    }

    impl TreeMergeBuilder {
        fn build_typed_for_tests(
            &self,
            config: TreeMergeConfig,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            build_tree_merge(config, inputs, cx, &CurrentOwnerMaterializer)
        }
    }

    fn build_context(root: &std::path::Path) -> BuildContext {
        let state_dir = root.join("tree");
        let temp_dir = state_dir.join("tmp");
        std::fs::create_dir_all(&state_dir).unwrap();
        mbuild_core::fsutil::recreate_empty_dir_force(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    fn sample_install() -> InstallMeta {
        InstallMeta {
            rules: vec![InstallRule {
                path: "**".to_string(),
                attrs: InstallAttrs {
                    uid: Some(0),
                    gid: Some(0),
                    directory_mode: Some(0o755),
                    regular_file_mode: Some(0o644),
                    executable_file_mode: Some(0o755),
                    symlink_mode: Some(0o777),
                },
            }],
        }
    }

    fn install_with_attrs(
        uid: u32,
        gid: u32,
        directory_mode: u32,
        regular_file_mode: u32,
    ) -> InstallMeta {
        InstallMeta {
            rules: vec![InstallRule {
                path: "**".to_string(),
                attrs: InstallAttrs {
                    uid: Some(uid),
                    gid: Some(gid),
                    directory_mode: Some(directory_mode),
                    regular_file_mode: Some(regular_file_mode),
                    executable_file_mode: Some(0o755),
                    symlink_mode: None,
                },
            }],
        }
    }

    fn install_with_modes(directory_mode: u32, regular_file_mode: u32) -> InstallMeta {
        install_with_attrs(0, 0, directory_mode, regular_file_mode)
    }

    fn fs_tree_root(result: &StagedBuildResult) -> PathBuf {
        result.staged_path.join("root")
    }

    fn fs_tree_manifest(result: &StagedBuildResult) -> FsTreeManifest {
        FsTreeManifest::read_canonical(&result.staged_path.join("manifest.jsonl")).unwrap()
    }

    fn assert_valid_fs_tree(result: &StagedBuildResult) {
        let owner_map = current_owner_map(&result.staged_path.join("root")).unwrap();
        validate_fs_tree_object(&result.staged_path, &owner_map).unwrap();
    }

    fn build_fs_tree_for_tests(
        root: &Path,
        name: &str,
        entries: Vec<TreeEntry>,
        install: InstallMeta,
    ) -> StagedBuildResult {
        let builder = TreeBuilder;
        let mut cx = build_context(&root.join(name));
        builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload { entries },
                    install: Some(install),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap()
    }

    fn tree_merge_inputs(inputs: &[(&str, &StagedBuildResult)]) -> BuilderInputs {
        let mut builder_inputs = BuilderInputs::empty();
        for (name, result) in inputs {
            builder_inputs.insert(
                *name,
                BuilderInputObject {
                    object_path: result.staged_path.clone(),
                    meta: Map::new(),
                },
            );
        }
        builder_inputs
    }

    fn fixed_object_hash() -> ObjectHash {
        "1111111111111111111111111111111111111111111111111111111111111111"
            .parse()
            .unwrap()
    }

    fn apply_test_modes(manifest: &FsTreeManifest, root_dir: &Path) -> Result<(), BuilderError> {
        for entry in manifest.entries() {
            if let FsTreeEntry::File { path, mode, .. } = entry {
                set_mode(&root_dir.join(path), *mode)?;
            }
        }
        apply_directory_modes_post_order(manifest, root_dir)
    }

    #[cfg(unix)]
    fn current_owner_map(path: &Path) -> Result<CurrentOwnerMap, BuilderError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to inspect fs-tree root '{}': {error}",
                path.display()
            ))
        })?;
        Ok(CurrentOwnerMap {
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }

    #[cfg(not(unix))]
    fn current_owner_map(_path: &Path) -> Result<CurrentOwnerMap, BuilderError> {
        Ok(CurrentOwnerMap { uid: 0, gid: 0 })
    }

    #[test]
    fn tree_merge_requires_at_least_two_inputs() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed_for_tests(TreeMergeConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires at least two fs-tree inputs")
        );
    }

    #[test]
    fn tree_merge_rejects_non_fs_tree_input() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::Dir {
                path: "left".to_string(),
            }],
            sample_install(),
        );
        let not_tree = temp.path().join("not-tree");
        fs::write(&not_tree, b"not a tree").unwrap();
        let mut inputs = tree_merge_inputs(&[("left", &left)]);
        inputs.insert(
            "bad",
            BuilderInputObject {
                object_path: not_tree,
                meta: Map::new(),
            },
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let error = builder
            .build_typed_for_tests(TreeMergeConfig {}, inputs, &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("is not a valid fs-tree object"));
    }

    #[test]
    fn tree_merge_combines_disjoint_fs_trees() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "bin/left".to_string(),
                text: "left\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "etc/right.conf".to_string(),
                text: "right\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let result = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("right", &right), ("left", &left)]),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(fs_tree_root(&result).join("bin/left")).unwrap(),
            "left\n"
        );
        assert_eq!(
            fs::read_to_string(fs_tree_root(&result).join("etc/right.conf")).unwrap(),
            "right\n"
        );
        assert!(
            fs_tree_manifest(&result)
                .entries()
                .iter()
                .any(|entry| entry.path() == "bin")
        );
        assert_valid_fs_tree(&result);
    }

    #[cfg(unix)]
    #[test]
    fn tree_merge_does_not_scan_input_directories_during_manifest_compose() {
        let temp = tempdir().unwrap();
        let base_object = temp.path().join("base");
        let base_manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("locked", 0, 0, 0o000),
        ])
        .unwrap();
        let base_paths = create_fs_tree_staging_dir(&base_object, &base_manifest).unwrap();
        fs::create_dir(base_paths.root_dir.join("locked")).unwrap();
        fs::set_permissions(
            base_paths.root_dir.join("locked"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "bin/right".to_string(),
                text: "right\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let mut inputs = tree_merge_inputs(&[("right", &right)]);
        inputs.insert(
            "base",
            BuilderInputObject {
                object_path: base_object,
                meta: Map::new(),
            },
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let result =
            build_tree_merge(TreeMergeConfig {}, inputs, &mut cx, &FixedHashMaterializer).unwrap();

        assert_eq!(result.object_hash, Some(fixed_object_hash()));
        assert!(fs_tree_root(&result).join("locked").is_dir());
        assert_eq!(
            fs::metadata(fs_tree_root(&result).join("locked"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o000
        );

        fs::set_permissions(
            base_paths.root_dir.join("locked"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::set_permissions(
            fs_tree_root(&result).join("locked"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tree_merge_rejects_hardlink_source_attr_mismatch_without_mutating_source() {
        let temp = tempdir().unwrap();
        let base_object = temp.path().join("base");
        let base_manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file("bin/tool", 0, 0, 0o755),
        ])
        .unwrap();
        let base_paths = create_fs_tree_staging_dir(&base_object, &base_manifest).unwrap();
        fs::create_dir(base_paths.root_dir.join("bin")).unwrap();
        fs::write(base_paths.root_dir.join("bin/tool"), b"tool").unwrap();
        fs::set_permissions(
            base_paths.root_dir.join("bin/tool"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "etc/right.conf".to_string(),
                text: "right\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let mut inputs = tree_merge_inputs(&[("right", &right)]);
        inputs.insert(
            "base",
            BuilderInputObject {
                object_path: base_object,
                meta: Map::new(),
            },
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let error = build_tree_merge(
            TreeMergeConfig {},
            inputs,
            &mut cx,
            &CurrentOwnerMaterializer,
        )
        .unwrap_err();

        assert!(error.to_string().contains("has mode 644, expected 755"));
        assert_eq!(
            fs::metadata(base_paths.root_dir.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644
        );
    }

    #[cfg(unix)]
    #[test]
    fn tree_merge_excludes_hardlinked_files_from_materialize_manifest() {
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "bin/left".to_string(),
                text: "left\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "etc/right.conf".to_string(),
                text: "right\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("merge"));
        let materializer = RecordingMaterializer {
            materialized_paths: RefCell::new(Vec::new()),
        };

        let result = build_tree_merge(
            TreeMergeConfig {},
            tree_merge_inputs(&[("right", &right), ("left", &left)]),
            &mut cx,
            &materializer,
        )
        .unwrap();

        assert_eq!(result.object_hash, Some(fixed_object_hash()));
        let materialized_paths = materializer.materialized_paths.borrow();
        assert!(materialized_paths.iter().any(|path| path == ""));
        assert!(materialized_paths.iter().any(|path| path == "bin"));
        assert!(materialized_paths.iter().any(|path| path == "etc"));
        assert!(!materialized_paths.iter().any(|path| path == "bin/left"));
        assert!(
            !materialized_paths
                .iter()
                .any(|path| path == "etc/right.conf")
        );
    }

    #[test]
    fn tree_merge_allows_matching_directory_overlap() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "usr/bin/left".to_string(),
                text: "left\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "usr/lib/right".to_string(),
                text: "right\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let result = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("left", &left), ("right", &right)]),
                &mut cx,
            )
            .unwrap();

        assert!(fs_tree_root(&result).join("usr/bin/left").is_file());
        assert!(fs_tree_root(&result).join("usr/lib/right").is_file());
        assert_valid_fs_tree(&result);
    }

    #[test]
    fn tree_merge_rejects_directory_attr_mismatch() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::Dir {
                path: "var".to_string(),
            }],
            install_with_modes(0o755, 0o644),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::Dir {
                path: "var".to_string(),
            }],
            install_with_modes(0o700, 0o644),
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let error = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("left", &left), ("right", &right)]),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("conflicting fs-tree entries"));
    }

    #[test]
    fn tree_merge_rejects_duplicate_files_and_symlinks() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left_file = build_fs_tree_for_tests(
            temp.path(),
            "left-file",
            vec![TreeEntry::File {
                path: "bin/tool".to_string(),
                text: "left\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let right_file = build_fs_tree_for_tests(
            temp.path(),
            "right-file",
            vec![TreeEntry::File {
                path: "bin/tool".to_string(),
                text: "right\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("merge-files"));
        let error = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("left", &left_file), ("right", &right_file)]),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("conflicting fs-tree entries"));

        let left_link = build_fs_tree_for_tests(
            temp.path(),
            "left-link",
            vec![TreeEntry::Symlink {
                path: "bin/tool".to_string(),
                target: "left".to_string(),
            }],
            sample_install(),
        );
        let right_link = build_fs_tree_for_tests(
            temp.path(),
            "right-link",
            vec![TreeEntry::Symlink {
                path: "bin/tool".to_string(),
                target: "right".to_string(),
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("merge-links"));
        let error = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("left", &left_link), ("right", &right_link)]),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("conflicting fs-tree entries"));
    }

    #[test]
    fn tree_merge_rejects_leaf_parent_conflicts() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let leaf = build_fs_tree_for_tests(
            temp.path(),
            "leaf",
            vec![
                TreeEntry::File {
                    path: "opt".to_string(),
                    text: "leaf\n".to_string(),
                    executable: false,
                },
                TreeEntry::Dir {
                    path: "other".to_string(),
                },
            ],
            sample_install(),
        );
        let child = build_fs_tree_for_tests(
            temp.path(),
            "child",
            vec![TreeEntry::File {
                path: "opt/tool".to_string(),
                text: "child\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let error = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("leaf", &leaf), ("child", &child)]),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("conflict"));
    }

    #[cfg(unix)]
    #[test]
    fn tree_merge_hardlinks_files_when_possible() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "bin/tool".to_string(),
                text: "tool\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::Dir {
                path: "etc".to_string(),
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let result = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("left", &left), ("right", &right)]),
                &mut cx,
            )
            .unwrap();

        let src = fs::metadata(fs_tree_root(&left).join("bin/tool")).unwrap();
        let dst = fs::metadata(fs_tree_root(&result).join("bin/tool")).unwrap();
        assert_eq!((src.dev(), src.ino()), (dst.dev(), dst.ino()));
    }

    #[test]
    fn single_file_tree_builds_file_object() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "hello.txt".to_string(),
                            text: "hello".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert!(result.meta.is_empty());
        assert!(result.staged_path.is_file());
        assert_eq!(fs::read_to_string(&result.staged_path).unwrap(), "hello");
    }

    #[test]
    fn single_nested_file_requires_directory_output() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("directory output requires install")
        );
    }

    #[test]
    fn single_dir_entry_produces_directory_output() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::Dir {
                            path: "dev".to_string(),
                        }],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert!(result.staged_path.is_dir());
        assert!(result.staged_path.join("manifest.jsonl").is_file());
        assert!(fs_tree_root(&result).join("dev").is_dir());
        assert!(result.meta.is_empty());
        assert_valid_fs_tree(&result);
    }

    #[test]
    fn materializes_explicit_empty_directories() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::Dir {
                                path: "dev".to_string(),
                            },
                            TreeEntry::Dir {
                                path: "proc".to_string(),
                            },
                            TreeEntry::Dir {
                                path: "sys".to_string(),
                            },
                        ],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        let root = fs_tree_root(&result);
        assert!(root.join("dev").is_dir());
        assert!(root.join("proc").is_dir());
        assert!(root.join("sys").is_dir());
        assert_eq!(fs::read_dir(root.join("dev")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.join("proc")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.join("sys")).unwrap().count(), 0);
        assert_valid_fs_tree(&result);
    }

    #[test]
    fn materializes_symlink_entries_with_literal_targets() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::Dir {
                                path: "usr/bin".to_string(),
                            },
                            TreeEntry::Symlink {
                                path: "bin".to_string(),
                                target: "usr/bin".to_string(),
                            },
                        ],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        let target = fs::read_link(fs_tree_root(&result).join("bin")).unwrap();
        assert_eq!(target, PathBuf::from("usr/bin"));
        assert_valid_fs_tree(&result);
    }

    #[test]
    fn materializes_broken_symlink_entries() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::Symlink {
                            path: "etc/mtab".to_string(),
                            target: "/proc/self/mounts".to_string(),
                        }],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        let target = fs::read_link(fs_tree_root(&result).join("etc/mtab")).unwrap();
        assert_eq!(target, PathBuf::from("/proc/self/mounts"));
        assert_valid_fs_tree(&result);
    }

    #[test]
    fn directory_tree_builds_fs_tree_with_empty_meta_and_manifest() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::Dir {
                                path: "dev".to_string(),
                            },
                            TreeEntry::File {
                                path: "etc/hostname".to_string(),
                                text: "mbuild\n".to_string(),
                                executable: false,
                            },
                            TreeEntry::File {
                                path: "init".to_string(),
                                text: "#!/bin/sh\n".to_string(),
                                executable: true,
                            },
                            TreeEntry::Symlink {
                                path: "bin".to_string(),
                                target: "usr/bin".to_string(),
                            },
                        ],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert!(result.staged_path.is_dir());
        let root = fs_tree_root(&result);
        assert!(root.join("dev").is_dir());
        assert_eq!(
            fs::read_to_string(root.join("etc/hostname")).unwrap(),
            "mbuild\n"
        );
        assert_eq!(
            fs::read_link(root.join("bin")).unwrap(),
            PathBuf::from("usr/bin")
        );
        assert!(result.meta.is_empty());

        let manifest = fs_tree_manifest(&result);
        assert_eq!(
            manifest.entries(),
            &[
                FsTreeEntry::directory("", 0, 0, 0o755),
                FsTreeEntry::symlink("bin", 0, 0, "usr/bin"),
                FsTreeEntry::directory("dev", 0, 0, 0o755),
                FsTreeEntry::directory("etc", 0, 0, 0o755),
                FsTreeEntry::file("etc/hostname", 0, 0, 0o644),
                FsTreeEntry::file("init", 0, 0, 0o755),
            ]
        );
        assert_valid_fs_tree(&result);

        #[cfg(unix)]
        {
            let init_mode = fs::metadata(root.join("init"))
                .unwrap()
                .permissions()
                .mode();
            let etc_mode = fs::metadata(root.join("etc")).unwrap().permissions().mode();
            assert_eq!(init_mode & 0o777, 0o755);
            assert_eq!(etc_mode & 0o777, 0o755);
        }
    }

    #[test]
    fn directory_tree_applies_restrictive_directory_modes_after_children() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "share/info.txt".to_string(),
                            text: "inline\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: Some(InstallMeta {
                        rules: vec![
                            InstallRule {
                                path: "**".to_string(),
                                attrs: InstallAttrs {
                                    uid: Some(0),
                                    gid: Some(0),
                                    directory_mode: Some(0o755),
                                    regular_file_mode: Some(0o644),
                                    executable_file_mode: Some(0o755),
                                    symlink_mode: None,
                                },
                            },
                            InstallRule {
                                path: "share/**".to_string(),
                                attrs: InstallAttrs {
                                    uid: None,
                                    gid: None,
                                    directory_mode: Some(0o555),
                                    regular_file_mode: Some(0o444),
                                    executable_file_mode: None,
                                    symlink_mode: None,
                                },
                            },
                        ],
                    }),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(fs_tree_root(&result).join("share/info.txt")).unwrap(),
            "inline\n"
        );
        assert_valid_fs_tree(&result);

        #[cfg(unix)]
        {
            let root = fs_tree_root(&result);
            let share_mode = fs::metadata(root.join("share"))
                .unwrap()
                .permissions()
                .mode();
            let file_mode = fs::metadata(root.join("share/info.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(share_mode & 0o777, 0o555);
            assert_eq!(file_mode & 0o777, 0o444);
        }
    }

    #[test]
    fn tree_fs_object_hash_changes_with_mode_bytes_and_symlink_target() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();

        let mut cx = build_context(temp.path());
        let base = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: Some(install_with_modes(0o755, 0o644)),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();
        let base_hash = fsobj_hash::hash_path(&base.staged_path).unwrap();

        let mut cx = build_context(temp.path());
        let changed_mode = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: Some(install_with_modes(0o700, 0o600)),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();
        assert_ne!(
            base_hash,
            fsobj_hash::hash_path(&changed_mode.staged_path).unwrap()
        );

        let mut cx = build_context(temp.path());
        let changed_bytes = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "other\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: Some(install_with_modes(0o755, 0o644)),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();
        assert_ne!(
            base_hash,
            fsobj_hash::hash_path(&changed_bytes.staged_path).unwrap()
        );

        let mut cx = build_context(temp.path());
        let link_a = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::Symlink {
                            path: "bin".to_string(),
                            target: "usr/bin".to_string(),
                        }],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();
        let link_a_hash = fsobj_hash::hash_path(&link_a.staged_path).unwrap();
        let mut cx = build_context(temp.path());
        let link_b = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::Symlink {
                            path: "bin".to_string(),
                            target: "sbin".to_string(),
                        }],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();
        assert_ne!(
            link_a_hash,
            fsobj_hash::hash_path(&link_b.staged_path).unwrap()
        );
    }

    #[test]
    fn file_output_rejects_install_metadata() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "hello.txt".to_string(),
                            text: "hello".to_string(),
                            executable: false,
                        }],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("file output must not specify install")
        );
    }

    #[test]
    fn directory_output_requires_install_metadata() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("directory output requires install")
        );
    }

    #[test]
    fn directory_output_rejects_empty_install_rules() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::Dir {
                            path: "dev".to_string(),
                        }],
                    },
                    install: Some(InstallMeta { rules: vec![] }),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("install.rules must contain at least one rule")
        );
    }

    #[test]
    fn directory_manifest_preserves_non_root_owner_attrs() {
        let entries = normalize_entries(vec![TreeEntry::File {
            path: "etc/hostname".to_string(),
            text: "mbuild\n".to_string(),
            executable: false,
        }])
        .unwrap();
        let rules = compile_install_rules(&install_with_attrs(42, 43, 0o755, 0o644).rules).unwrap();

        let manifest = fs_tree_manifest_for_entries(&entries, &rules).unwrap();

        assert!(
            manifest
                .entries()
                .contains(&FsTreeEntry::directory("etc", 42, 43, 0o755))
        );
        assert!(
            manifest
                .entries()
                .contains(&FsTreeEntry::file("etc/hostname", 42, 43, 0o644))
        );
    }

    #[test]
    fn partial_ownerless_overrides_inherit_non_root_owner_attrs() {
        let entries = normalize_entries(vec![TreeEntry::File {
            path: "etc/hostname".to_string(),
            text: "mbuild\n".to_string(),
            executable: false,
        }])
        .unwrap();
        let install = InstallMeta {
            rules: vec![
                InstallRule {
                    path: "**".to_string(),
                    attrs: InstallAttrs {
                        uid: Some(42),
                        gid: Some(43),
                        directory_mode: Some(0o755),
                        regular_file_mode: Some(0o644),
                        executable_file_mode: Some(0o755),
                        symlink_mode: None,
                    },
                },
                InstallRule {
                    path: "etc/**".to_string(),
                    attrs: InstallAttrs {
                        uid: None,
                        gid: None,
                        directory_mode: Some(0o700),
                        regular_file_mode: Some(0o600),
                        executable_file_mode: None,
                        symlink_mode: None,
                    },
                },
            ],
        };
        let rules = compile_install_rules(&install.rules).unwrap();

        let manifest = fs_tree_manifest_for_entries(&entries, &rules).unwrap();

        assert!(
            manifest
                .entries()
                .contains(&FsTreeEntry::directory("etc", 42, 43, 0o700))
        );
        assert!(
            manifest
                .entries()
                .contains(&FsTreeEntry::file("etc/hostname", 42, 43, 0o600))
        );
    }

    #[test]
    fn directory_output_rejects_uncovered_paths_and_missing_attrs() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: Some(InstallMeta {
                        rules: vec![InstallRule {
                            path: "bin/**".to_string(),
                            attrs: InstallAttrs {
                                uid: Some(0),
                                gid: Some(0),
                                directory_mode: Some(0o755),
                                regular_file_mode: Some(0o644),
                                executable_file_mode: Some(0o755),
                                symlink_mode: None,
                            },
                        }],
                    }),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is not covered by any install rule")
        );

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        }],
                    },
                    install: Some(InstallMeta {
                        rules: vec![InstallRule {
                            path: "**".to_string(),
                            attrs: InstallAttrs {
                                uid: Some(0),
                                gid: None,
                                directory_mode: Some(0o755),
                                regular_file_mode: Some(0o644),
                                executable_file_mode: Some(0o755),
                                symlink_mode: None,
                            },
                        }],
                    }),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("missing resolved gid"));
    }

    #[test]
    fn tree_builder_rejects_non_empty_inputs() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "unexpected",
            BuilderInputObject {
                object_path: std::path::PathBuf::from("/tmp/unexpected"),
                meta: Map::new(),
            },
        );

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "hello.txt".to_string(),
                            text: "hello".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
    }

    #[test]
    fn rejects_invalid_and_conflicting_paths() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::File {
                                path: "etc".to_string(),
                                text: "bad".to_string(),
                                executable: false,
                            },
                            TreeEntry::File {
                                path: "etc/hostname".to_string(),
                                text: "mbuild\n".to_string(),
                                executable: false,
                            },
                        ],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("conflicts with descendant path 'etc/hostname'")
        );

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "../escape".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("must not contain '..'"));

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::Dir {
                                path: "etc".to_string(),
                            },
                            TreeEntry::File {
                                path: "etc".to_string(),
                                text: "bad".to_string(),
                                executable: false,
                            },
                        ],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("duplicate tree entry path 'etc'")
        );

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "/etc/hostname".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("must be relative"));

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "./etc/hostname".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("must not contain '.'"));

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc\\hostname".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("must use '/' separators"));

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "etc//hostname".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        }],
                    },
                    install: None,
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("must not contain empty segments")
        );

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::Symlink {
                                path: "bin".to_string(),
                                target: "usr/bin".to_string(),
                            },
                            TreeEntry::File {
                                path: "bin/tool".to_string(),
                                text: "bad".to_string(),
                                executable: false,
                            },
                        ],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("symlink entry 'bin' conflicts with descendant path 'bin/tool'")
        );

        let error = builder
            .build_typed_for_tests(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::Symlink {
                                path: "bin".to_string(),
                                target: "".to_string(),
                            },
                            TreeEntry::File {
                                path: "etc/hostname".to_string(),
                                text: "mbuild\n".to_string(),
                                executable: false,
                            },
                        ],
                    },
                    install: Some(sample_install()),
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("symlink target must not be empty")
        );
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_erased(
                serde_json::json!({
                    "tree": {
                        "entries": [
                            {
                                "type": "file",
                                "path": "hello.txt",
                                "text": "hello",
                                "executable": false
                            }
                        ]
                    },
                    "extra": true
                }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }
}
