use fsobj_hash::{EntryKind, ObjectHash, hash_file_bytes, hash_path, hash_symlink_node};
use globset::{Glob, GlobMatcher};
#[cfg(test)]
use mbuild_core::InitramfsEntrySource;
use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    ComposedFsTree, ComposedFsTreeEntry, FsTreeComposeInput, FsTreeEntry, FsTreeManifest,
    FsTreeOwnerMap, StagedBuildResult, TypedBuilder, compose_fs_trees, create_fs_tree_staging_dir,
    fsutil,
};
use mbuild_runtime::{
    FsTreeInitramfsEntrySource, FsTreeInitramfsInput, FsTreeTarEntrySource, FsTreeTarInput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io;
#[cfg(test)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Instant;

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
    File { text: String, executable: bool },
    Directory,
    Symlink { target: String },
}

#[derive(Debug, Clone)]
struct IndexedTreeMergeInput {
    compose: FsTreeComposeInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputLeafIdentity {
    kind: EntryKind,
    node_hash: ObjectHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenInputLeaf {
    entry: FsTreeEntry,
    identity: InputLeafIdentity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TreeMergeStageStats {
    directory_count: usize,
    file_count: usize,
    hardlinked_file_count: usize,
    copied_file_count: usize,
    symlink_count: usize,
    directory_ms: u128,
    file_validate_ms: u128,
    hardlink_ms: u128,
    copy_ms: u128,
    symlink_ms: u128,
}

trait OwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        object_dir: &Path,
        manifest: &FsTreeManifest,
        temp_dir: &Path,
    ) -> Result<(), BuilderError>;

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
pub struct TreeSubsetBuilder;
pub struct TreeMergeBuilder;
pub struct ErofsRootfsBuilder;
pub struct InitramfsBuilder;

static TREE_SPEC: BuilderSpec = BuilderSpec {
    tag: "Tree",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeMergeConfig {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeSubsetConfig {
    include: Vec<String>,
}

static TREE_MERGE_SPEC: BuilderSpec = BuilderSpec {
    tag: "TreeMerge",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

static TREE_SUBSET_SPEC: BuilderSpec = BuilderSpec {
    tag: "TreeSubset",
    required_inputs: &["tree"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErofsRootfsConfig {
    #[serde(default)]
    compression: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

static EROFS_ROOTFS_SPEC: BuilderSpec = BuilderSpec {
    tag: "ErofsRootfs",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitramfsConfig {}

static INITRAMFS_SPEC: BuilderSpec = BuilderSpec {
    tag: "Initramfs",
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

impl TypedBuilder for TreeSubsetBuilder {
    type Config = TreeSubsetConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &TREE_SUBSET_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree_subset(
            config,
            inputs,
            cx,
            &RuntimeOwnershipMaterializer,
            &StdTreeSubsetLinker,
        )
    }
}

impl TypedBuilder for ErofsRootfsBuilder {
    type Config = ErofsRootfsConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &EROFS_ROOTFS_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_erofs_rootfs(
            config,
            inputs,
            cx,
            &RuntimeErofsTarWriter,
            &PathProgramResolver,
        )
    }
}

impl TypedBuilder for InitramfsBuilder {
    type Config = InitramfsConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &INITRAMFS_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_initramfs(config, inputs, cx, &RuntimeInitramfsWriter)
    }
}

#[derive(Debug)]
struct CompiledTreeSubsetPattern {
    pattern: String,
    matcher: GlobMatcher,
}

trait TreeSubsetLinker {
    fn hard_link(&self, source: &Path, dst: &Path) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy)]
struct StdTreeSubsetLinker;

impl TreeSubsetLinker for StdTreeSubsetLinker {
    fn hard_link(&self, source: &Path, dst: &Path) -> io::Result<()> {
        fs::hard_link(source, dst)
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

    let object_hash = match output_kind {
        OutputKind::File => {
            materialize_file_output(&output_path, &normalized)?;
            None
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
            Some(object_hash)
        }
    };

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash,
    })
}

fn build_tree_subset(
    config: TreeSubsetConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    materializer: &impl OwnershipMaterializer,
    linker: &impl TreeSubsetLinker,
) -> Result<StagedBuildResult, BuilderError> {
    let input = tree_subset_input(inputs.required("tree")?)?;
    let patterns = compile_tree_subset_patterns(&config.include)?;

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!(
            "selecting subset from fs-tree input with {} include pattern(s)",
            patterns.len()
        ),
    );

    let compose_start = Instant::now();
    let composed = compose_tree_subset(&input.compose, &patterns)?;
    let entry_count = composed.manifest().entries().len();
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "compose-done",
        format!("selected {entry_count} fs-tree entries"),
        None,
        None,
        tree_subset_compose_details(patterns.len(), entry_count, elapsed_ms(compose_start)),
    );

    let now_nanos = fsutil::current_epoch_nanos()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let output_path = cx.temp_dir.join(format!("tree-subset-{now_nanos}.obj"));

    cx.log_event(
        BuildLogLevel::Info,
        "materialize",
        format!("materializing fs-tree subset '{}'", output_path.display()),
    );

    let temp_dir = cx.temp_dir.clone();
    let object_hash = materialize_tree_subset_output(
        &output_path,
        &composed,
        &temp_dir,
        materializer,
        linker,
        cx,
    )?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: Some(object_hash),
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

    let compose_start = Instant::now();
    let merge_inputs = inputs
        .iter()
        .map(|(name, object)| tree_merge_input(name, object))
        .collect::<Result<Vec<_>, _>>()?;
    let composed =
        compose_rootfs_inputs_allowing_identical_leaf_overlap("TreeMerge", &merge_inputs)?;
    let entry_count = composed.manifest().entries().len();
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "compose-done",
        format!(
            "composed {} fs-tree input(s) into {} entries",
            inputs.len(),
            entry_count
        ),
        None,
        None,
        tree_merge_compose_details(inputs.len(), entry_count, elapsed_ms(compose_start)),
    );

    let now_nanos = fsutil::current_epoch_nanos()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let output_path = cx.temp_dir.join(format!("tree-merge-{now_nanos}.obj"));

    cx.log_event(
        BuildLogLevel::Info,
        "materialize",
        format!("materializing merged fs-tree '{}'", output_path.display()),
    );

    let temp_dir = cx.temp_dir.clone();
    let object_hash =
        materialize_tree_merge_output(&output_path, &composed, &temp_dir, materializer, cx)?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: Some(object_hash),
    })
}

fn build_erofs_rootfs(
    config: ErofsRootfsConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    tar_writer: &impl ErofsTarWriter,
    program_resolver: &impl ProgramResolver,
) -> Result<StagedBuildResult, BuilderError> {
    validate_erofs_config(&config)?;
    let inputs = inputs.extras(&EROFS_ROOTFS_SPEC).collect::<Vec<_>>();
    if inputs.is_empty() {
        return Err(BuilderError::ExecutionFailed(
            "ErofsRootfs builder requires at least one fs-tree input".to_string(),
        ));
    }

    let mkfs_erofs = program_resolver.resolve("mkfs.erofs")?;

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!("composing {} fs-tree input(s) for EROFS", inputs.len()),
    );

    let merge_inputs = inputs
        .iter()
        .map(|(name, object)| erofs_rootfs_input(name, object))
        .collect::<Result<Vec<_>, _>>()?;
    let compose_inputs = merge_inputs
        .iter()
        .map(|input| input.compose.clone())
        .collect::<Vec<_>>();
    let composed =
        compose_rootfs_inputs_allowing_identical_leaf_overlap("ErofsRootfs", &merge_inputs)?;

    let now_nanos = fsutil::current_epoch_nanos()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let tar_path = cx.temp_dir.join(format!("erofs-rootfs-{now_nanos}.tar"));
    let output_path = cx.temp_dir.join(format!("erofs-rootfs-{now_nanos}.erofs"));

    cx.log_event(
        BuildLogLevel::Info,
        "tar",
        format!(
            "writing deterministic EROFS source tar '{}'",
            tar_path.display()
        ),
    );
    tar_writer.write_tar(&compose_inputs, &composed, &tar_path, &cx.temp_dir)?;

    cx.log_event(
        BuildLogLevel::Info,
        "mkfs",
        format!("creating EROFS image '{}'", output_path.display()),
    );
    run_mkfs_erofs(&mkfs_erofs, &config, &output_path, &tar_path)?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: None,
    })
}

fn build_initramfs(
    _config: InitramfsConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
    initramfs_writer: &impl InitramfsWriter,
) -> Result<StagedBuildResult, BuilderError> {
    let inputs = inputs.extras(&INITRAMFS_SPEC).collect::<Vec<_>>();
    if inputs.is_empty() {
        return Err(BuilderError::ExecutionFailed(
            "Initramfs builder requires at least one fs-tree input".to_string(),
        ));
    }

    cx.log_event(
        BuildLogLevel::Info,
        "prepare",
        format!("composing {} fs-tree input(s) for initramfs", inputs.len()),
    );

    let merge_inputs = inputs
        .iter()
        .map(|(name, object)| initramfs_input(name, object))
        .collect::<Result<Vec<_>, _>>()?;
    let compose_inputs = merge_inputs
        .iter()
        .map(|input| input.compose.clone())
        .collect::<Vec<_>>();
    let composed =
        compose_rootfs_inputs_allowing_identical_leaf_overlap("Initramfs", &merge_inputs)?;

    let now_nanos = fsutil::current_epoch_nanos()
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
    let output_path = cx.temp_dir.join(format!("initramfs-{now_nanos}.img"));

    cx.log_event(
        BuildLogLevel::Info,
        "initramfs",
        format!(
            "writing deterministic initramfs '{}'",
            output_path.display()
        ),
    );
    initramfs_writer.write_initramfs(&compose_inputs, &composed, &output_path, &cx.temp_dir)?;

    Ok(StagedBuildResult {
        staged_path: output_path,
        object_hash: None,
    })
}

fn validate_erofs_config(config: &ErofsRootfsConfig) -> Result<(), BuilderError> {
    if matches!(config.compression.as_deref(), Some("")) {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: compression must be null or a non-empty string".to_string(),
        ));
    }
    if matches!(config.label.as_deref(), Some("")) {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: label must be null or a non-empty string".to_string(),
        ));
    }
    Ok(())
}

fn erofs_rootfs_input(
    name: &str,
    object: &BuilderInputObject,
) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.object_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "ErofsRootfs input '{name}' is not a valid fs-tree object: {error}"
        ))
    })?;
    let _ = object;
    Ok(IndexedTreeMergeInput { compose })
}

fn initramfs_input(
    name: &str,
    object: &BuilderInputObject,
) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.object_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "Initramfs input '{name}' is not a valid fs-tree object: {error}"
        ))
    })?;
    let _ = object;
    Ok(IndexedTreeMergeInput { compose })
}

trait ErofsTarWriter {
    fn write_tar(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_tar: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError>;
}

trait InitramfsWriter {
    fn write_initramfs(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_initramfs: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError>;
}

#[derive(Debug, Clone, Copy)]
struct RuntimeInitramfsWriter;

impl InitramfsWriter for RuntimeInitramfsWriter {
    fn write_initramfs(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_initramfs: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError> {
        let idmap = mbuild_runtime::cached_host_idmap()
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        let initramfs_inputs = inputs
            .iter()
            .map(|input| FsTreeInitramfsInput {
                root_dir: input.root_dir.clone(),
            })
            .collect::<Vec<_>>();
        let sources = initramfs_sources(inputs, composed)?;
        mbuild_runtime::write_fs_tree_initramfs_in_ownership_namespace(
            &initramfs_inputs,
            composed.manifest(),
            &sources,
            output_initramfs,
            idmap.as_ref(),
            workspace,
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
    }
}

#[derive(Debug, Clone, Copy)]
struct RuntimeErofsTarWriter;

impl ErofsTarWriter for RuntimeErofsTarWriter {
    fn write_tar(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_tar: &Path,
        workspace: &Path,
    ) -> Result<(), BuilderError> {
        let idmap = mbuild_runtime::cached_host_idmap()
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        let tar_inputs = inputs
            .iter()
            .map(|input| FsTreeTarInput {
                root_dir: input.root_dir.clone(),
            })
            .collect::<Vec<_>>();
        let sources = erofs_tar_sources(inputs, composed)?;
        mbuild_runtime::write_fs_tree_tar_in_ownership_namespace(
            &tar_inputs,
            composed.manifest(),
            &sources,
            output_tar,
            idmap.as_ref(),
            workspace,
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
    }
}

fn erofs_tar_sources(
    inputs: &[FsTreeComposeInput],
    composed: &ComposedFsTree,
) -> Result<Vec<FsTreeTarEntrySource>, BuilderError> {
    composed
        .manifest()
        .entries()
        .iter()
        .zip(composed.entries())
        .map(
            |(manifest_entry, composed_entry)| match (manifest_entry, composed_entry) {
                (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory) => {
                    Ok(FsTreeTarEntrySource::Directory)
                }
                (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { source_path }) => {
                    let (input_index, rel_path) = locate_erofs_source(inputs, source_path)?;
                    Ok(FsTreeTarEntrySource::File {
                        input_index,
                        path: rel_path,
                    })
                }
                (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                    Ok(FsTreeTarEntrySource::Symlink)
                }
                _ => Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                ))),
            },
        )
        .collect()
}

fn initramfs_sources(
    inputs: &[FsTreeComposeInput],
    composed: &ComposedFsTree,
) -> Result<Vec<FsTreeInitramfsEntrySource>, BuilderError> {
    composed
        .manifest()
        .entries()
        .iter()
        .zip(composed.entries())
        .map(
            |(manifest_entry, composed_entry)| match (manifest_entry, composed_entry) {
                (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory) => {
                    Ok(FsTreeInitramfsEntrySource::Directory)
                }
                (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { source_path }) => {
                    let (input_index, rel_path) = locate_rootfs_source(inputs, source_path)?;
                    Ok(FsTreeInitramfsEntrySource::File {
                        input_index,
                        path: rel_path,
                    })
                }
                (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                    Ok(FsTreeInitramfsEntrySource::Symlink)
                }
                _ => Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                ))),
            },
        )
        .collect()
}

fn locate_erofs_source(
    inputs: &[FsTreeComposeInput],
    source_path: &Path,
) -> Result<(usize, String), BuilderError> {
    locate_rootfs_source(inputs, source_path).map_err(|error| {
        BuilderError::ExecutionFailed(error.to_string().replace("rootfs", "ErofsRootfs"))
    })
}

fn locate_rootfs_source(
    inputs: &[FsTreeComposeInput],
    source_path: &Path,
) -> Result<(usize, String), BuilderError> {
    for (index, input) in inputs.iter().enumerate() {
        if let Ok(rel_path) = source_path.strip_prefix(&input.root_dir) {
            let rel_path = rel_path_to_manifest_string(rel_path).ok_or_else(|| {
                BuilderError::ExecutionFailed(format!(
                    "fs-tree source path '{}' is not representable as a manifest path",
                    source_path.display()
                ))
            })?;
            if rel_path.is_empty() {
                return Err(BuilderError::ExecutionFailed(format!(
                    "fs-tree source path '{}' points at an input root, expected a file",
                    source_path.display()
                )));
            }
            return Ok((index, rel_path));
        }
    }
    Err(BuilderError::ExecutionFailed(format!(
        "fs-tree source path '{}' is not under any rootfs input root",
        source_path.display()
    )))
}

fn rel_path_to_manifest_string(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
fn write_composed_fs_tree_tar_host(
    composed: &ComposedFsTree,
    output_tar: &Path,
) -> Result<(), BuilderError> {
    let file = fs::File::create(output_tar).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create EROFS source tar '{}': {error}",
            output_tar.display()
        ))
    })?;
    write_composed_fs_tree_tar_stream(file, composed)
}

#[cfg(test)]
fn write_composed_fs_tree_initramfs_host(
    composed: &ComposedFsTree,
    output_initramfs: &Path,
) -> Result<(), BuilderError> {
    let file = fs::File::create(output_initramfs).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create initramfs '{}': {error}",
            output_initramfs.display()
        ))
    })?;
    let sources = initramfs_host_sources(composed)?;
    mbuild_core::write_newc_initramfs(file, composed.manifest().entries(), &sources)
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
}

#[cfg(test)]
fn initramfs_host_sources(
    composed: &ComposedFsTree,
) -> Result<Vec<InitramfsEntrySource>, BuilderError> {
    composed
        .manifest()
        .entries()
        .iter()
        .zip(composed.entries())
        .map(
            |(manifest_entry, composed_entry)| match (manifest_entry, composed_entry) {
                (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory) => {
                    Ok(InitramfsEntrySource::Directory)
                }
                (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { source_path }) => {
                    Ok(InitramfsEntrySource::File {
                        path: source_path.clone(),
                    })
                }
                (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                    Ok(InitramfsEntrySource::Symlink)
                }
                _ => Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                ))),
            },
        )
        .collect()
}

#[cfg(test)]
fn write_composed_fs_tree_tar_stream<W: Write>(
    writer: W,
    composed: &ComposedFsTree,
) -> Result<(), BuilderError> {
    let mut tar = tar::Builder::new(io::BufWriter::new(writer));
    tar.mode(tar::HeaderMode::Deterministic);

    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(composed.entries())
    {
        if manifest_entry.path().is_empty() {
            continue;
        }
        match (manifest_entry, composed_entry) {
            (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory) => {
                append_erofs_tar_directory(&mut tar, manifest_entry)?
            }
            (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { source_path }) => {
                append_erofs_tar_file(&mut tar, manifest_entry, source_path)?
            }
            (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                append_erofs_tar_symlink(&mut tar, manifest_entry)?
            }
            _ => {
                return Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                )));
            }
        }
    }

    let mut writer = tar.into_inner().map_err(|error| {
        BuilderError::ExecutionFailed(format!("failed to finalize EROFS source tar: {error}"))
    })?;
    writer.flush().map_err(|error| {
        BuilderError::ExecutionFailed(format!("failed to flush EROFS source tar: {error}"))
    })?;
    Ok(())
}

#[cfg(test)]
fn append_erofs_tar_directory<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
) -> Result<(), BuilderError> {
    let FsTreeEntry::Directory {
        path,
        uid,
        gid,
        mode,
    } = entry
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: expected directory manifest entry".to_string(),
        ));
    };
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(*mode);
    header.set_mtime(0);
    header.set_size(0);
    header.set_cksum();
    tar.append_data(&mut header, format!("{path}/"), io::empty())
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to append EROFS source tar directory '{path}': {error}"
            ))
        })
}

#[cfg(test)]
fn append_erofs_tar_file<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
    source_path: &Path,
) -> Result<(), BuilderError> {
    let FsTreeEntry::File {
        path,
        uid,
        gid,
        mode,
        ..
    } = entry
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: expected file manifest entry".to_string(),
        ));
    };
    let metadata = fs::metadata(source_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to stat EROFS source file '{}': {error}",
            source_path.display()
        ))
    })?;
    let mut file = fs::File::open(source_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to open EROFS source file '{}': {error}",
            source_path.display()
        ))
    })?;
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(*mode);
    header.set_mtime(0);
    header.set_size(metadata.len());
    header.set_cksum();
    tar.append_data(&mut header, path, &mut file)
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to append EROFS source tar file '{path}': {error}"
            ))
        })
}

#[cfg(test)]
fn append_erofs_tar_symlink<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
) -> Result<(), BuilderError> {
    let FsTreeEntry::Symlink {
        path,
        uid,
        gid,
        target,
        ..
    } = entry
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: expected symlink manifest entry".to_string(),
        ));
    };
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(0o777);
    header.set_mtime(0);
    header.set_size(0);
    header.set_link_name(target).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to encode EROFS source tar symlink target '{target}' for '{path}': {error}"
        ))
    })?;
    header.set_cksum();
    tar.append_data(&mut header, path, io::empty())
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to append EROFS source tar symlink '{path}': {error}"
            ))
        })
}

trait ProgramResolver {
    fn resolve(&self, program: &str) -> Result<PathBuf, BuilderError>;
}

#[derive(Debug, Clone, Copy)]
struct PathProgramResolver;

impl ProgramResolver for PathProgramResolver {
    fn resolve(&self, program: &str) -> Result<PathBuf, BuilderError> {
        find_program_in_path(program).ok_or_else(|| {
            BuilderError::ExecutionFailed(format!(
                "required tool '{program}' was not found in PATH; install erofs-utils"
            ))
        })
    }
}

fn find_program_in_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable_file(&path).then_some(path);
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn run_mkfs_erofs(
    mkfs_erofs: &Path,
    config: &ErofsRootfsConfig,
    output_path: &Path,
    tar_path: &Path,
) -> Result<(), BuilderError> {
    let mut command = Command::new(mkfs_erofs);
    command
        .arg("--tar=f")
        .arg("--sort=path")
        .arg("-T")
        .arg("0")
        .arg("-U")
        .arg("clear");
    if let Some(label) = config.label.as_ref() {
        command.arg("-L").arg(label);
    }
    if let Some(compression) = config.compression.as_ref() {
        command.arg("-z").arg(compression);
    }
    command.arg(output_path).arg(tar_path);

    let output = command.output().map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to execute '{}': {error}",
            mkfs_erofs.display()
        ))
    })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(BuilderError::ExecutionFailed(format!(
        "mkfs.erofs failed with status {}: {}",
        output.status,
        stderr.trim_end()
    )))
}

fn tree_merge_input(
    name: &str,
    object: &BuilderInputObject,
) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.object_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "TreeMerge input '{name}' is not a valid fs-tree object: {error}"
        ))
    })?;
    let _ = object;
    Ok(IndexedTreeMergeInput { compose })
}

fn tree_subset_input(object: &BuilderInputObject) -> Result<IndexedTreeMergeInput, BuilderError> {
    let compose = load_fs_tree_compose_input(&object.object_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "TreeSubset input 'tree' is not a valid fs-tree object: {error}"
        ))
    })?;
    let _ = object;
    Ok(IndexedTreeMergeInput { compose })
}

fn compose_rootfs_inputs_allowing_identical_leaf_overlap(
    builder_name: &str,
    inputs: &[IndexedTreeMergeInput],
) -> Result<ComposedFsTree, BuilderError> {
    let mut seen_leaves = BTreeMap::<String, SeenInputLeaf>::new();
    let mut compose_inputs = Vec::with_capacity(inputs.len());

    for input in inputs {
        let mut entries = Vec::new();
        for entry in input.compose.manifest.entries() {
            if is_fs_tree_leaf(entry) {
                let identity = input_leaf_identity(builder_name, entry)?;
                if let Some(seen) = seen_leaves.get(entry.path()) {
                    ensure_identical_leaf_overlap(entry, identity, seen)?;
                    continue;
                }
                seen_leaves.insert(
                    entry.path().to_string(),
                    SeenInputLeaf {
                        entry: entry.clone(),
                        identity,
                    },
                );
            }
            entries.push(entry.clone());
        }

        compose_inputs.push(FsTreeComposeInput {
            manifest: FsTreeManifest::from_entries(entries).map_err(map_fs_tree_error)?,
            root_dir: input.compose.root_dir.clone(),
        });
    }

    compose_fs_trees(&compose_inputs).map_err(map_fs_tree_error)
}

fn input_leaf_identity(
    builder_name: &str,
    entry: &FsTreeEntry,
) -> Result<InputLeafIdentity, BuilderError> {
    let expected_kind = leaf_entry_kind_for_manifest_entry(entry).ok_or_else(|| {
        BuilderError::ExecutionFailed(format!(
            "fs-tree path '{}' is not a file or symlink",
            entry.path()
        ))
    })?;
    let Some(node_hash) = entry.leaf_hash() else {
        return Err(BuilderError::ExecutionFailed(format!(
            "{builder_name} input manifest leaf '{}' is missing hash",
            entry.path()
        )));
    };

    Ok(InputLeafIdentity {
        kind: expected_kind,
        node_hash,
    })
}

fn ensure_identical_leaf_overlap(
    entry: &FsTreeEntry,
    identity: InputLeafIdentity,
    seen: &SeenInputLeaf,
) -> Result<(), BuilderError> {
    if seen.entry == *entry && seen.identity == identity {
        return Ok(());
    }

    let reason = if leaf_entry_kind(&seen.entry) != leaf_entry_kind(entry) {
        format!(
            "{} vs {}",
            leaf_entry_kind(&seen.entry),
            leaf_entry_kind(entry)
        )
    } else if seen.entry != *entry {
        "metadata differs".to_string()
    } else {
        "leaf hash differs".to_string()
    };
    Err(BuilderError::ExecutionFailed(format!(
        "conflicting fs-tree entries at '{}': duplicate leaf entries differ ({reason})",
        entry.path()
    )))
}

fn is_fs_tree_leaf(entry: &FsTreeEntry) -> bool {
    matches!(
        entry,
        FsTreeEntry::File { .. } | FsTreeEntry::Symlink { .. }
    )
}

fn leaf_entry_kind_for_manifest_entry(entry: &FsTreeEntry) -> Option<EntryKind> {
    match entry {
        FsTreeEntry::File { .. } => Some(EntryKind::File),
        FsTreeEntry::Symlink { .. } => Some(EntryKind::Symlink),
        FsTreeEntry::Directory { .. } => None,
    }
}

fn leaf_entry_kind(entry: &FsTreeEntry) -> &'static str {
    match entry {
        FsTreeEntry::File { .. } => "file",
        FsTreeEntry::Symlink { .. } => "symlink",
        FsTreeEntry::Directory { .. } => "directory",
    }
}

fn compile_tree_subset_patterns(
    patterns: &[String],
) -> Result<Vec<CompiledTreeSubsetPattern>, BuilderError> {
    if patterns.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: include must contain at least one pattern".to_string(),
        ));
    }

    patterns
        .iter()
        .map(|pattern| {
            validate_tree_subset_pattern(pattern)?;
            let glob = Glob::new(pattern).map_err(|error| {
                BuilderError::InvalidRecipe(format!(
                    "invalid builder config: invalid include pattern '{}': {error}",
                    pattern
                ))
            })?;
            Ok(CompiledTreeSubsetPattern {
                pattern: pattern.clone(),
                matcher: glob.compile_matcher(),
            })
        })
        .collect()
}

fn validate_tree_subset_pattern(pattern: &str) -> Result<(), BuilderError> {
    if pattern.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: include pattern must not be empty".to_string(),
        ));
    }
    let path = Path::new(pattern);
    if path.is_absolute() {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: include pattern '{pattern}' must be relative"
        )));
    }
    if pattern.contains('\\') {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: include pattern '{pattern}' must use '/' separators"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: include pattern '{pattern}' must not contain '..'"
        )));
    }
    Ok(())
}

fn compose_tree_subset(
    input: &FsTreeComposeInput,
    patterns: &[CompiledTreeSubsetPattern],
) -> Result<ComposedFsTree, BuilderError> {
    let mut by_path = BTreeMap::new();
    for entry in input.manifest.entries() {
        by_path.insert(entry.path(), entry);
    }

    let mut selected = BTreeSet::<String>::new();
    for entry in input.manifest.entries() {
        let path = entry.path();
        if path.is_empty() {
            continue;
        }
        if tree_subset_path_matches(path, patterns) {
            selected.insert(path.to_string());
            add_tree_subset_parent_dirs(path, &by_path, &mut selected)?;
        }
    }

    if !selected.iter().any(|path| !path.is_empty()) {
        return Err(BuilderError::InvalidRecipe(
            "invalid builder config: include patterns selected no paths".to_string(),
        ));
    }

    let mut manifest_entries = Vec::new();
    for entry in input.manifest.entries() {
        if selected.contains(entry.path()) {
            manifest_entries.push(entry.clone());
        }
    }
    let manifest = FsTreeManifest::from_entries(manifest_entries).map_err(map_fs_tree_error)?;
    let entries = manifest
        .entries()
        .iter()
        .map(|entry| match entry {
            FsTreeEntry::Directory { .. } => ComposedFsTreeEntry::Directory,
            FsTreeEntry::File { path, .. } => ComposedFsTreeEntry::File {
                source_path: input.root_dir.join(path),
            },
            FsTreeEntry::Symlink { path, .. } => ComposedFsTreeEntry::Symlink {
                source_path: input.root_dir.join(path),
            },
        })
        .collect();

    Ok(ComposedFsTree { manifest, entries })
}

fn tree_subset_path_matches(path: &str, patterns: &[CompiledTreeSubsetPattern]) -> bool {
    patterns.iter().any(|pattern| {
        pattern.matcher.is_match(path)
            || pattern
                .pattern
                .strip_suffix("/**")
                .is_some_and(|prefix| path == prefix)
    })
}

fn add_tree_subset_parent_dirs(
    path: &str,
    by_path: &BTreeMap<&str, &FsTreeEntry>,
    selected: &mut BTreeSet<String>,
) -> Result<(), BuilderError> {
    selected.insert(String::new());
    let mut remainder = path;
    while let Some((parent, _)) = remainder.rsplit_once('/') {
        let entry = by_path.get(parent).ok_or_else(|| {
            BuilderError::ExecutionFailed(format!(
                "input fs-tree manifest is missing parent directory '{parent}' for '{path}'"
            ))
        })?;
        if !matches!(entry, FsTreeEntry::Directory { .. }) {
            return Err(BuilderError::ExecutionFailed(format!(
                "input fs-tree manifest parent '{parent}' for '{path}' is not a directory"
            )));
        }
        selected.insert(parent.to_string());
        remainder = parent;
    }
    Ok(())
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
    cx: &mut BuildContext,
) -> Result<ObjectHash, BuilderError> {
    let paths =
        create_fs_tree_staging_dir(output_path, composed.manifest()).map_err(map_fs_tree_error)?;
    let mut materialize_entries = Vec::new();
    let mut stats = TreeMergeStageStats::default();

    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(composed.entries())
    {
        match (manifest_entry, composed_entry) {
            (FsTreeEntry::Directory { path, .. }, ComposedFsTreeEntry::Directory) => {
                let step_start = Instant::now();
                if !path.is_empty() {
                    fs::create_dir(paths.root_dir.join(path)).map_err(|error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to create merged fs-tree directory '{}': {error}",
                            paths.root_dir.join(path).display()
                        ))
                    })?;
                }
                stats.directory_ms += elapsed_ms(step_start);
                stats.directory_count += 1;
                materialize_entries.push(manifest_entry.clone());
            }
            (FsTreeEntry::File { path, .. }, ComposedFsTreeEntry::File { source_path }) => {
                let dst = paths.root_dir.join(path);
                let result =
                    link_or_copy_tree_merge_file(source_path, &dst, manifest_entry, materializer)?;
                stats.file_count += 1;
                stats.file_validate_ms += result.validate_ms;
                stats.hardlink_ms += result.hardlink_ms;
                stats.copy_ms += result.copy_ms;
                match result.kind {
                    TreeMergeFileMaterialization::Hardlinked => {
                        stats.hardlinked_file_count += 1;
                    }
                    TreeMergeFileMaterialization::Copied => {
                        stats.copied_file_count += 1;
                    }
                }
                if result.kind == TreeMergeFileMaterialization::Copied {
                    materialize_entries.push(manifest_entry.clone());
                }
            }
            (
                FsTreeEntry::Symlink { path, target, .. },
                ComposedFsTreeEntry::Symlink { source_path },
            ) => {
                let dst = paths.root_dir.join(path);
                let step_start = Instant::now();
                create_tree_merge_symlink(source_path, &dst, target)?;
                stats.symlink_ms += elapsed_ms(step_start);
                stats.symlink_count += 1;
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

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "stage-done",
        format!(
            "staged merged fs-tree with {} entries",
            composed.manifest().entries().len()
        ),
        None,
        None,
        tree_merge_stage_details(composed.manifest().entries().len(), &stats),
    );

    let materialize_manifest =
        FsTreeManifest::from_entries(materialize_entries).map_err(map_fs_tree_error)?;
    let ownership_start = Instant::now();
    materializer.materialize_and_validate(
        &paths.root_dir,
        &paths.object_dir,
        &materialize_manifest,
        temp_dir,
    )?;
    let ownership_host_ms = elapsed_ms(ownership_start);
    log_tree_merge_ownership_events(cx, ownership_host_ms);

    let hash_start = Instant::now();
    let object_hash = hash_tree_output_from_manifest(composed)?;
    log_tree_merge_hash_event(cx, object_hash, elapsed_ms(hash_start), 0);
    Ok(object_hash)
}

fn materialize_tree_subset_output(
    output_path: &Path,
    composed: &ComposedFsTree,
    temp_dir: &Path,
    materializer: &impl OwnershipMaterializer,
    linker: &impl TreeSubsetLinker,
    cx: &mut BuildContext,
) -> Result<ObjectHash, BuilderError> {
    let paths =
        create_fs_tree_staging_dir(output_path, composed.manifest()).map_err(map_fs_tree_error)?;
    let mut materialize_entries = Vec::new();
    let mut stats = TreeMergeStageStats::default();

    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(composed.entries())
    {
        match (manifest_entry, composed_entry) {
            (FsTreeEntry::Directory { path, .. }, ComposedFsTreeEntry::Directory) => {
                let step_start = Instant::now();
                if !path.is_empty() {
                    fs::create_dir(paths.root_dir.join(path)).map_err(|error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to create fs-tree subset directory '{}': {error}",
                            paths.root_dir.join(path).display()
                        ))
                    })?;
                }
                stats.directory_ms += elapsed_ms(step_start);
                stats.directory_count += 1;
                materialize_entries.push(manifest_entry.clone());
            }
            (FsTreeEntry::File { path, .. }, ComposedFsTreeEntry::File { source_path }) => {
                let dst = paths.root_dir.join(path);
                let result = hardlink_tree_subset_file(
                    source_path,
                    &dst,
                    manifest_entry,
                    materializer,
                    linker,
                )?;
                stats.file_count += 1;
                stats.file_validate_ms += result.validate_ms;
                stats.hardlink_ms += result.hardlink_ms;
                stats.hardlinked_file_count += 1;
            }
            (
                FsTreeEntry::Symlink { path, target, .. },
                ComposedFsTreeEntry::Symlink { source_path },
            ) => {
                let dst = paths.root_dir.join(path);
                let step_start = Instant::now();
                create_tree_merge_symlink(source_path, &dst, target)?;
                stats.symlink_ms += elapsed_ms(step_start);
                stats.symlink_count += 1;
                materialize_entries.push(manifest_entry.clone());
            }
            _ => {
                return Err(BuilderError::ExecutionFailed(format!(
                    "fs-tree subset entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                )));
            }
        }
    }

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "stage-done",
        format!(
            "staged fs-tree subset with {} entries",
            composed.manifest().entries().len()
        ),
        None,
        None,
        tree_merge_stage_details(composed.manifest().entries().len(), &stats),
    );

    let materialize_manifest =
        FsTreeManifest::from_entries(materialize_entries).map_err(map_fs_tree_error)?;
    let ownership_start = Instant::now();
    materializer.materialize_and_validate(
        &paths.root_dir,
        &paths.object_dir,
        &materialize_manifest,
        temp_dir,
    )?;
    let ownership_host_ms = elapsed_ms(ownership_start);
    log_tree_merge_ownership_events(cx, ownership_host_ms);

    let hash_start = Instant::now();
    let object_hash = hash_tree_output_from_manifest(composed)?;
    log_tree_merge_hash_event(cx, object_hash, elapsed_ms(hash_start), 0);
    Ok(object_hash)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeMergeFileMaterialization {
    Hardlinked,
    Copied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TreeMergeFileMaterializationResult {
    kind: TreeMergeFileMaterialization,
    validate_ms: u128,
    hardlink_ms: u128,
    copy_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TreeSubsetFileMaterializationResult {
    validate_ms: u128,
    hardlink_ms: u128,
}

fn hardlink_tree_subset_file(
    source: &Path,
    dst: &Path,
    manifest_entry: &FsTreeEntry,
    materializer: &impl OwnershipMaterializer,
    linker: &impl TreeSubsetLinker,
) -> Result<TreeSubsetFileMaterializationResult, BuilderError> {
    let validate_start = Instant::now();
    materializer.validate_hardlinked_file(source, manifest_entry)?;
    let validate_ms = elapsed_ms(validate_start);
    let hardlink_start = Instant::now();
    linker.hard_link(source, dst).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to hardlink fs-tree subset file '{}' to '{}': {error}",
            source.display(),
            dst.display()
        ))
    })?;
    Ok(TreeSubsetFileMaterializationResult {
        validate_ms,
        hardlink_ms: elapsed_ms(hardlink_start),
    })
}

fn link_or_copy_tree_merge_file(
    source: &Path,
    dst: &Path,
    manifest_entry: &FsTreeEntry,
    materializer: &impl OwnershipMaterializer,
) -> Result<TreeMergeFileMaterializationResult, BuilderError> {
    let validate_start = Instant::now();
    materializer.validate_hardlinked_file(source, manifest_entry)?;
    let validate_ms = elapsed_ms(validate_start);
    let hardlink_start = Instant::now();
    match fs::hard_link(source, dst) {
        Ok(()) => Ok(TreeMergeFileMaterializationResult {
            kind: TreeMergeFileMaterialization::Hardlinked,
            validate_ms,
            hardlink_ms: elapsed_ms(hardlink_start),
            copy_ms: 0,
        }),
        Err(error) if should_copy_after_link_error(error.kind()) => {
            let hardlink_ms = elapsed_ms(hardlink_start);
            let copy_start = Instant::now();
            fs::copy(source, dst).map_err(|copy_error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to copy merged fs-tree file '{}' to '{}': {copy_error}",
                    source.display(),
                    dst.display()
                ))
            })?;
            Ok(TreeMergeFileMaterializationResult {
                kind: TreeMergeFileMaterialization::Copied,
                validate_ms,
                hardlink_ms,
                copy_ms: elapsed_ms(copy_start),
            })
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

fn hash_tree_output_from_manifest(composed: &ComposedFsTree) -> Result<ObjectHash, BuilderError> {
    mbuild_core::hash_fs_tree_object_from_manifest(composed.manifest()).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to hash fs-tree object from manifest: {error}"
        ))
    })
}

fn log_tree_merge_ownership_events(cx: &mut BuildContext, host_duration_ms: u128) {
    let mut ownership_details = Map::new();
    ownership_details.insert("duration_ms".to_string(), json_u128(host_duration_ms));
    ownership_details.insert("host_duration_ms".to_string(), json_u128(host_duration_ms));
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "ownership-done",
        format!("materialized ownership in {host_duration_ms} ms"),
        None,
        None,
        ownership_details,
    );
}

fn log_tree_merge_hash_event(
    cx: &mut BuildContext,
    object_hash: ObjectHash,
    hash_ms: u128,
    manifest_serialize_ms: u128,
) {
    let mut hash_details = Map::new();
    hash_details.insert("duration_ms".to_string(), json_u128(hash_ms));
    hash_details.insert(
        "manifest_serialize_ms".to_string(),
        json_u128(manifest_serialize_ms),
    );
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "hash-done",
        format!("hashed merged fs-tree object in {hash_ms} ms"),
        Some(object_hash),
        None,
        hash_details,
    );
}

fn tree_merge_compose_details(
    input_count: usize,
    entry_count: usize,
    duration_ms: u128,
) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("duration_ms".to_string(), json_u128(duration_ms));
    details.insert("input_count".to_string(), json_usize(input_count));
    details.insert("entry_count".to_string(), json_usize(entry_count));
    details
}

fn tree_subset_compose_details(
    pattern_count: usize,
    entry_count: usize,
    duration_ms: u128,
) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("duration_ms".to_string(), json_u128(duration_ms));
    details.insert("pattern_count".to_string(), json_usize(pattern_count));
    details.insert("entry_count".to_string(), json_usize(entry_count));
    details
}

fn tree_merge_stage_details(entry_count: usize, stats: &TreeMergeStageStats) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert(
        "duration_ms".to_string(),
        json_u128(
            stats.directory_ms
                + stats.file_validate_ms
                + stats.hardlink_ms
                + stats.copy_ms
                + stats.symlink_ms,
        ),
    );
    details.insert("entry_count".to_string(), json_usize(entry_count));
    details.insert(
        "directory_count".to_string(),
        json_usize(stats.directory_count),
    );
    details.insert("file_count".to_string(), json_usize(stats.file_count));
    details.insert(
        "hardlinked_file_count".to_string(),
        json_usize(stats.hardlinked_file_count),
    );
    details.insert(
        "copied_file_count".to_string(),
        json_usize(stats.copied_file_count),
    );
    details.insert("symlink_count".to_string(), json_usize(stats.symlink_count));
    details.insert("directory_ms".to_string(), json_u128(stats.directory_ms));
    details.insert(
        "file_validate_ms".to_string(),
        json_u128(stats.file_validate_ms),
    );
    details.insert("hardlink_ms".to_string(), json_u128(stats.hardlink_ms));
    details.insert("copy_ms".to_string(), json_u128(stats.copy_ms));
    details.insert("symlink_ms".to_string(), json_u128(stats.symlink_ms));
    details
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

fn json_usize(value: usize) -> Value {
    Value::from(value as u64)
}

fn json_u128(value: u128) -> Value {
    Value::from(value.min(u64::MAX as u128) as u64)
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
        ..
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
                text,
                executable,
            } => fs_tree_entry_for_path(
                rel_path,
                MaterializedKind::File {
                    text: text.clone(),
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
        MaterializedKind::File { text, executable } => {
            let mode = if executable {
                required_attr(attrs.executable_file_mode, rel_path, "executable_file_mode")?
            } else {
                required_attr(attrs.regular_file_mode, rel_path, "regular_file_mode")?
            };
            Ok(FsTreeEntry::file_with_hash(
                rel_path,
                uid,
                gid,
                mode,
                hash_file_bytes(mode & 0o111 != 0, text.as_bytes()),
            ))
        }
        MaterializedKind::Symlink { target } => {
            let hash = hash_symlink_node(target.as_bytes());
            Ok(FsTreeEntry::symlink_with_hash(
                rel_path, uid, gid, target, hash,
            ))
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
        BuildLogEvent, BuildLogger, Builder, BuilderInputObject, BuilderInputs, FsTreeObjectError,
        FsTreeOwnerMap, validate_fs_tree_object,
    };
    use std::cell::RefCell;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[derive(Debug, Clone, Copy)]
    struct CurrentOwnerMaterializer;

    #[derive(Debug, Clone, Copy)]
    struct FixedHashMaterializer;

    #[derive(Debug)]
    struct RecordingMaterializer {
        materialized_paths: RefCell<Vec<String>>,
    }

    #[derive(Debug, Clone, Copy)]
    struct FailingTreeSubsetLinker {
        kind: io::ErrorKind,
    }

    #[derive(Debug, Default)]
    struct RecordingBuildLogger {
        events: Mutex<Vec<BuildLogEvent>>,
    }

    impl RecordingBuildLogger {
        fn events(&self) -> Vec<BuildLogEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl BuildLogger for RecordingBuildLogger {
        fn log_event(&self, event: BuildLogEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn allocate_raw_log_path(&self, _label: &str) -> Result<PathBuf, String> {
            Err("recording logger does not allocate raw logs".to_string())
        }
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
            root_dir: &Path,
            _object_dir: &Path,
            manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<(), BuilderError> {
            apply_test_modes(manifest, root_dir)?;
            Ok(())
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
            manifest: &FsTreeManifest,
            _temp_dir: &Path,
        ) -> Result<(), BuilderError> {
            self.materialized_paths.replace(
                manifest
                    .entries()
                    .iter()
                    .map(|entry| entry.path().to_string())
                    .collect(),
            );
            Ok(())
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

    impl TreeSubsetLinker for FailingTreeSubsetLinker {
        fn hard_link(&self, _source: &Path, _dst: &Path) -> io::Result<()> {
            Err(io::Error::from(self.kind))
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

    impl TreeSubsetBuilder {
        fn build_typed_for_tests(
            &self,
            config: TreeSubsetConfig,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            build_tree_subset(
                config,
                inputs,
                cx,
                &CurrentOwnerMaterializer,
                &StdTreeSubsetLinker,
            )
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

    impl ErofsRootfsBuilder {
        fn build_typed_for_tests(
            &self,
            config: ErofsRootfsConfig,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
            mkfs_erofs: PathBuf,
        ) -> Result<StagedBuildResult, BuilderError> {
            build_erofs_rootfs(
                config,
                inputs,
                cx,
                &HostErofsTarWriter,
                &FixedProgramResolver { path: mkfs_erofs },
            )
        }
    }

    impl InitramfsBuilder {
        fn build_typed_for_tests(
            &self,
            config: InitramfsConfig,
            inputs: BuilderInputs,
            cx: &mut BuildContext,
        ) -> Result<StagedBuildResult, BuilderError> {
            build_initramfs(config, inputs, cx, &HostInitramfsWriter)
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct HostErofsTarWriter;

    impl ErofsTarWriter for HostErofsTarWriter {
        fn write_tar(
            &self,
            _inputs: &[FsTreeComposeInput],
            composed: &ComposedFsTree,
            output_tar: &Path,
            _workspace: &Path,
        ) -> Result<(), BuilderError> {
            write_composed_fs_tree_tar_host(composed, output_tar)
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct HostInitramfsWriter;

    impl InitramfsWriter for HostInitramfsWriter {
        fn write_initramfs(
            &self,
            _inputs: &[FsTreeComposeInput],
            composed: &ComposedFsTree,
            output_initramfs: &Path,
            _workspace: &Path,
        ) -> Result<(), BuilderError> {
            write_composed_fs_tree_initramfs_host(composed, output_initramfs)
        }
    }

    #[derive(Debug, Clone)]
    struct FixedProgramResolver {
        path: PathBuf,
    }

    impl ProgramResolver for FixedProgramResolver {
        fn resolve(&self, _program: &str) -> Result<PathBuf, BuilderError> {
            Ok(self.path.clone())
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct MissingProgramResolver;

    impl ProgramResolver for MissingProgramResolver {
        fn resolve(&self, program: &str) -> Result<PathBuf, BuilderError> {
            Err(BuilderError::ExecutionFailed(format!(
                "required tool '{program}' was not found in PATH; install erofs-utils"
            )))
        }
    }

    fn build_context(root: &std::path::Path) -> BuildContext {
        let state_dir = root.join("tree");
        let temp_dir = state_dir.join("tmp");
        std::fs::create_dir_all(&state_dir).unwrap();
        mbuild_core::fsutil::recreate_empty_dir_force(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    fn build_context_with_recording_logger(
        root: &std::path::Path,
    ) -> (BuildContext, Arc<RecordingBuildLogger>) {
        let logger = Arc::new(RecordingBuildLogger::default());
        let cx = build_context(root).with_logger(logger.clone());
        (cx, logger)
    }

    fn detail_u64(event: &BuildLogEvent, key: &str) -> u64 {
        event
            .details
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("missing numeric detail '{key}' in {event:?}"))
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
        if let Some(object_hash) = result.object_hash {
            assert_eq!(object_hash, hash_path(&result.staged_path).unwrap());
        }
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
                    object_hash: result
                        .object_hash
                        .unwrap_or_else(|| hash_path(&result.staged_path).unwrap()),
                },
            );
        }
        builder_inputs
    }

    fn install_fake_mkfs_erofs(dir: &Path, log_path: &Path, fail: bool) -> PathBuf {
        let script_path = dir.join("mkfs.erofs");
        let failure = if fail {
            "echo simulated mkfs failure >&2\nexit 17\n"
        } else {
            ""
        };
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > {}\n{failure}last=''\nprev=''\nfor arg in \"$@\"; do\n  prev=\"$last\"\n  last=\"$arg\"\ndone\nprintf 'fake erofs image\\n' > \"$prev\"\n",
                shell_quote(log_path)
            ),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
        script_path
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    fn read_fake_mkfs_args(log_path: &Path) -> Vec<String> {
        fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn tree_subset_config(patterns: &[&str]) -> TreeSubsetConfig {
        TreeSubsetConfig {
            include: patterns.iter().map(|pattern| pattern.to_string()).collect(),
        }
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
    fn tree_subset_selects_manifest_paths_and_recreates_symlinks() {
        let builder = TreeSubsetBuilder;
        let temp = tempdir().unwrap();
        let input = build_fs_tree_for_tests(
            temp.path(),
            "input",
            vec![
                TreeEntry::File {
                    path: "usr/lib64/libfoo.so.1".to_string(),
                    text: "runtime\n".to_string(),
                    executable: true,
                },
                TreeEntry::Symlink {
                    path: "usr/lib64/libfoo.so".to_string(),
                    target: "libfoo.so.1".to_string(),
                },
                TreeEntry::File {
                    path: "usr/bin/tool".to_string(),
                    text: "tool\n".to_string(),
                    executable: true,
                },
            ],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("subset"));

        let result = builder
            .build_typed_for_tests(
                tree_subset_config(&["usr/lib64/libfoo.so*", "not-present/**"]),
                tree_merge_inputs(&[("tree", &input)]),
                &mut cx,
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(fs_tree_root(&result).join("usr/lib64/libfoo.so.1")).unwrap(),
            "runtime\n"
        );
        assert_eq!(
            fs::read_link(fs_tree_root(&result).join("usr/lib64/libfoo.so")).unwrap(),
            PathBuf::from("libfoo.so.1")
        );
        assert!(!fs_tree_root(&result).join("usr/bin/tool").exists());
        assert!(fs_tree_root(&result).join("usr").is_dir());
        assert!(fs_tree_root(&result).join("usr/lib64").is_dir());
        assert_valid_fs_tree(&result);

        let manifest_paths = fs_tree_manifest(&result)
            .entries()
            .iter()
            .map(|entry| entry.path().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            manifest_paths,
            vec![
                "",
                "usr",
                "usr/lib64",
                "usr/lib64/libfoo.so",
                "usr/lib64/libfoo.so.1"
            ]
        );
        for path in ["usr/lib64/libfoo.so", "usr/lib64/libfoo.so.1"] {
            assert!(
                fs_tree_manifest(&result)
                    .entries()
                    .iter()
                    .find(|entry| entry.path() == path)
                    .and_then(FsTreeEntry::leaf_hash)
                    .is_some()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn tree_subset_hardlinks_selected_files() {
        let builder = TreeSubsetBuilder;
        let temp = tempdir().unwrap();
        let input = build_fs_tree_for_tests(
            temp.path(),
            "input",
            vec![TreeEntry::File {
                path: "lib/libfoo.so.1".to_string(),
                text: "runtime\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("subset"));

        let result = builder
            .build_typed_for_tests(
                tree_subset_config(&["lib/libfoo.so*"]),
                tree_merge_inputs(&[("tree", &input)]),
                &mut cx,
            )
            .unwrap();

        let src = fs::metadata(fs_tree_root(&input).join("lib/libfoo.so.1")).unwrap();
        let dst = fs::metadata(fs_tree_root(&result).join("lib/libfoo.so.1")).unwrap();
        assert_eq!((src.dev(), src.ino()), (dst.dev(), dst.ino()));
    }

    #[test]
    fn tree_subset_rejects_empty_result_but_allows_unmatched_patterns() {
        let builder = TreeSubsetBuilder;
        let temp = tempdir().unwrap();
        let input = build_fs_tree_for_tests(
            temp.path(),
            "input",
            vec![TreeEntry::File {
                path: "lib/libfoo.so.1".to_string(),
                text: "runtime\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("empty"));

        let error = builder
            .build_typed_for_tests(
                tree_subset_config(&["not-present/**"]),
                tree_merge_inputs(&[("tree", &input)]),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("selected no paths"));
    }

    #[test]
    fn tree_subset_rejects_invalid_config_and_input() {
        let builder = TreeSubsetBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(&temp.path().join("missing"));
        let error = builder
            .build_typed_for_tests(
                tree_subset_config(&["lib/*"]),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("required input slot 'tree' is missing")
        );

        let input = build_fs_tree_for_tests(
            temp.path(),
            "input",
            vec![TreeEntry::Dir {
                path: "lib".to_string(),
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("bad-pattern"));
        let error = builder
            .build_typed_for_tests(
                tree_subset_config(&["../lib/*"]),
                tree_merge_inputs(&[("tree", &input)]),
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("must not contain '..'"));

        let not_tree = temp.path().join("not-tree");
        fs::write(&not_tree, b"not a tree").unwrap();
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "tree",
            BuilderInputObject {
                object_hash: hash_path(&not_tree).unwrap(),
                object_path: not_tree,
            },
        );
        let mut cx = build_context(&temp.path().join("not-tree-cx"));
        let error = builder
            .build_typed_for_tests(tree_subset_config(&["lib/*"]), inputs, &mut cx)
            .unwrap_err();
        assert!(error.to_string().contains("is not a valid fs-tree object"));
    }

    #[test]
    fn tree_subset_fails_when_hardlink_fails() {
        let temp = tempdir().unwrap();
        let input = build_fs_tree_for_tests(
            temp.path(),
            "input",
            vec![TreeEntry::File {
                path: "lib/libfoo.so.1".to_string(),
                text: "runtime\n".to_string(),
                executable: true,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("subset"));

        let error = build_tree_subset(
            tree_subset_config(&["lib/libfoo.so*"]),
            tree_merge_inputs(&[("tree", &input)]),
            &mut cx,
            &CurrentOwnerMaterializer,
            &FailingTreeSubsetLinker {
                kind: io::ErrorKind::PermissionDenied,
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to hardlink fs-tree subset file")
        );
    }

    #[test]
    fn tree_subset_uses_manifest_without_discovering_input_tree() {
        let builder = TreeSubsetBuilder;
        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("input-object");
        let manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file_with_hash("bin/tool", 0, 0, 0o755, hash_file_bytes(true, b"tool\n")),
            FsTreeEntry::directory("locked", 0, 0, 0o000),
        ])
        .unwrap();
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        fs::create_dir(paths.root_dir.join("bin")).unwrap();
        fs::write(paths.root_dir.join("bin/tool"), b"tool\n").unwrap();
        fs::set_permissions(
            paths.root_dir.join("bin/tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "tree",
            BuilderInputObject {
                object_hash: hash_path(&object_dir).unwrap(),
                object_path: object_dir,
            },
        );
        let mut cx = build_context(&temp.path().join("subset"));

        let result = builder
            .build_typed_for_tests(tree_subset_config(&["bin/tool"]), inputs, &mut cx)
            .unwrap();

        assert_eq!(
            fs::read_to_string(fs_tree_root(&result).join("bin/tool")).unwrap(),
            "tool\n"
        );
        assert!(!fs_tree_root(&result).join("locked").exists());
        assert_valid_fs_tree(&result);
    }

    #[test]
    fn erofs_rootfs_invokes_mkfs_without_optional_flags_and_publishes_file() {
        let builder = ErofsRootfsBuilder;
        let temp = tempdir().unwrap();
        let tree = build_fs_tree_for_tests(
            temp.path(),
            "tree",
            vec![TreeEntry::File {
                path: "etc/hostname".to_string(),
                text: "mbuild\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let log_path = temp.path().join("mkfs-args.txt");
        let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, false);
        let mut cx = build_context(&temp.path().join("erofs"));

        let result = builder
            .build_typed_for_tests(
                ErofsRootfsConfig {
                    compression: None,
                    label: None,
                },
                tree_merge_inputs(&[("tree", &tree)]),
                &mut cx,
                mkfs,
            )
            .unwrap();

        assert!(result.staged_path.is_file());
        assert_eq!(
            fs::read_to_string(&result.staged_path).unwrap(),
            "fake erofs image\n"
        );
        assert_eq!(result.object_hash, None);
        let args = read_fake_mkfs_args(&log_path);
        assert_eq!(
            &args[..6],
            ["--tar=f", "--sort=path", "-T", "0", "-U", "clear"]
        );
        assert!(!args.iter().any(|arg| arg == "-z"));
        assert!(!args.iter().any(|arg| arg == "-L"));
        assert!(args[args.len() - 2].ends_with(".erofs"));
        assert!(args[args.len() - 1].ends_with(".tar"));
    }

    #[test]
    fn initramfs_writes_newc_file() {
        let builder = InitramfsBuilder;
        let temp = tempdir().unwrap();
        let tree = build_fs_tree_for_tests(
            temp.path(),
            "tree",
            vec![
                TreeEntry::File {
                    path: "init".to_string(),
                    text: "#!/bin/sh\n".to_string(),
                    executable: true,
                },
                TreeEntry::Symlink {
                    path: "bin/sh".to_string(),
                    target: "../init".to_string(),
                },
            ],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("initramfs"));

        let result = builder
            .build_typed_for_tests(
                InitramfsConfig {},
                tree_merge_inputs(&[("tree", &tree)]),
                &mut cx,
            )
            .unwrap();

        assert!(result.staged_path.is_file());
        assert_eq!(result.object_hash, None);
        let bytes = fs::read(&result.staged_path).unwrap();
        assert!(bytes.starts_with(b"070701"));
        assert!(
            bytes
                .windows("#!/bin/sh\n".len())
                .any(|window| window == b"#!/bin/sh\n")
        );
        assert!(
            bytes
                .windows("TRAILER!!!".len())
                .any(|window| window == b"TRAILER!!!")
        );
    }

    #[test]
    fn initramfs_allows_identical_duplicate_file_overlap() {
        let builder = InitramfsBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "etc/same".to_string(),
                text: "same\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "etc/same".to_string(),
                text: "same\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("initramfs"));

        let result = builder
            .build_typed_for_tests(
                InitramfsConfig {},
                tree_merge_inputs(&[("left", &left), ("right", &right)]),
                &mut cx,
            )
            .unwrap();

        assert!(result.staged_path.is_file());
    }

    #[test]
    fn initramfs_rejects_zero_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(&temp.path().join("initramfs"));

        let error = InitramfsBuilder
            .build_typed_for_tests(InitramfsConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("requires at least one"));
    }

    #[test]
    fn erofs_rootfs_allows_identical_duplicate_file_overlap() {
        let builder = ErofsRootfsBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "usr/lib64/libsame.so.1".to_string(),
                text: "same\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "usr/lib64/libsame.so.1".to_string(),
                text: "same\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let log_path = temp.path().join("mkfs-args.txt");
        let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, false);
        let mut cx = build_context(&temp.path().join("erofs"));

        let result = builder
            .build_typed_for_tests(
                ErofsRootfsConfig {
                    compression: None,
                    label: None,
                },
                tree_merge_inputs(&[("left", &left), ("right", &right)]),
                &mut cx,
                mkfs,
            )
            .unwrap();

        assert!(result.staged_path.is_file());
        assert_eq!(
            fs::read_to_string(&result.staged_path).unwrap(),
            "fake erofs image\n"
        );
    }

    #[test]
    fn erofs_rootfs_passes_compression_and_label_to_mkfs() {
        let builder = ErofsRootfsBuilder;
        let temp = tempdir().unwrap();
        let tree = build_fs_tree_for_tests(
            temp.path(),
            "tree",
            vec![TreeEntry::Dir {
                path: "var".to_string(),
            }],
            sample_install(),
        );
        let log_path = temp.path().join("mkfs-args.txt");
        let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, false);
        let mut cx = build_context(&temp.path().join("erofs"));

        builder
            .build_typed_for_tests(
                ErofsRootfsConfig {
                    compression: Some("lz4hc,12".to_string()),
                    label: Some("rootfs".to_string()),
                },
                tree_merge_inputs(&[("tree", &tree)]),
                &mut cx,
                mkfs,
            )
            .unwrap();

        let args = read_fake_mkfs_args(&log_path);
        assert!(args.windows(2).any(|window| window == ["-L", "rootfs"]));
        assert!(args.windows(2).any(|window| window == ["-z", "lz4hc,12"]));
    }

    #[test]
    fn erofs_rootfs_rejects_zero_inputs() {
        let temp = tempdir().unwrap();
        let mkfs = install_fake_mkfs_erofs(temp.path(), &temp.path().join("mkfs-args.txt"), false);
        let mut cx = build_context(&temp.path().join("erofs"));

        let error = ErofsRootfsBuilder
            .build_typed_for_tests(
                ErofsRootfsConfig {
                    compression: None,
                    label: None,
                },
                BuilderInputs::empty(),
                &mut cx,
                mkfs,
            )
            .unwrap_err();

        assert!(error.to_string().contains("requires at least one"));
    }

    #[test]
    fn erofs_rootfs_rejects_empty_compression_and_label() {
        let temp = tempdir().unwrap();
        let mkfs = install_fake_mkfs_erofs(temp.path(), &temp.path().join("mkfs-args.txt"), false);
        let mut cx = build_context(&temp.path().join("compression"));

        let error = ErofsRootfsBuilder
            .build_typed_for_tests(
                ErofsRootfsConfig {
                    compression: Some(String::new()),
                    label: None,
                },
                BuilderInputs::empty(),
                &mut cx,
                mkfs.clone(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("compression"));

        let mut cx = build_context(&temp.path().join("label"));
        let error = ErofsRootfsBuilder
            .build_typed_for_tests(
                ErofsRootfsConfig {
                    compression: None,
                    label: Some(String::new()),
                },
                BuilderInputs::empty(),
                &mut cx,
                mkfs,
            )
            .unwrap_err();
        assert!(error.to_string().contains("label"));
    }

    #[test]
    fn erofs_rootfs_reports_missing_mkfs_erofs() {
        let temp = tempdir().unwrap();
        let tree = build_fs_tree_for_tests(
            temp.path(),
            "tree",
            vec![TreeEntry::Dir {
                path: "var".to_string(),
            }],
            sample_install(),
        );
        let mut cx = build_context(&temp.path().join("erofs"));

        let error = build_erofs_rootfs(
            ErofsRootfsConfig {
                compression: None,
                label: None,
            },
            tree_merge_inputs(&[("tree", &tree)]),
            &mut cx,
            &HostErofsTarWriter,
            &MissingProgramResolver,
        )
        .unwrap_err();

        assert!(error.to_string().contains("mkfs.erofs"));
        assert!(error.to_string().contains("erofs-utils"));
    }

    #[test]
    fn erofs_rootfs_reports_mkfs_stderr_on_failure() {
        let temp = tempdir().unwrap();
        let tree = build_fs_tree_for_tests(
            temp.path(),
            "tree",
            vec![TreeEntry::Dir {
                path: "var".to_string(),
            }],
            sample_install(),
        );
        let log_path = temp.path().join("mkfs-args.txt");
        let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, true);
        let mut cx = build_context(&temp.path().join("erofs"));

        let error = ErofsRootfsBuilder
            .build_typed_for_tests(
                ErofsRootfsConfig {
                    compression: None,
                    label: None,
                },
                tree_merge_inputs(&[("tree", &tree)]),
                &mut cx,
                mkfs,
            )
            .unwrap_err();

        assert!(error.to_string().contains("mkfs.erofs failed"));
        assert!(error.to_string().contains("simulated mkfs failure"));
    }

    #[test]
    fn erofs_tar_generation_uses_manifest_metadata_order_and_sources() {
        let temp = tempdir().unwrap();
        let left_manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("usr", 11, 12, 0o755),
            FsTreeEntry::directory("usr/bin", 11, 12, 0o755),
            FsTreeEntry::file("usr/bin/tool", 11, 12, 0o755),
        ])
        .unwrap();
        let left_paths =
            create_fs_tree_staging_dir(&temp.path().join("left"), &left_manifest).unwrap();
        fs::create_dir(left_paths.root_dir.join("usr")).unwrap();
        fs::create_dir(left_paths.root_dir.join("usr/bin")).unwrap();
        fs::write(left_paths.root_dir.join("usr/bin/tool"), b"tool\n").unwrap();

        let right_manifest = FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("etc", 21, 22, 0o750),
            FsTreeEntry::file("etc/config", 21, 22, 0o640),
            FsTreeEntry::symlink("etc/tool-link", 21, 22, "../usr/bin/tool"),
        ])
        .unwrap();
        let right_paths =
            create_fs_tree_staging_dir(&temp.path().join("right"), &right_manifest).unwrap();
        fs::create_dir(right_paths.root_dir.join("etc")).unwrap();
        fs::write(right_paths.root_dir.join("etc/config"), b"config\n").unwrap();
        create_symlink(
            "../usr/bin/tool",
            &right_paths.root_dir.join("etc/tool-link"),
        )
        .unwrap();
        let compose_inputs = vec![
            load_fs_tree_compose_input(&right_paths.object_dir).unwrap(),
            load_fs_tree_compose_input(&left_paths.object_dir).unwrap(),
        ];
        let composed = compose_fs_trees(&compose_inputs).unwrap();
        let mut bytes = Vec::new();

        write_composed_fs_tree_tar_stream(&mut bytes, &composed).unwrap();

        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut contents = Vec::new();
            io::copy(&mut entry, &mut contents).unwrap();
            seen.push((
                path,
                header.entry_type(),
                header.uid().unwrap(),
                header.gid().unwrap(),
                header.mode().unwrap(),
                header.mtime().unwrap(),
                contents,
                header.link_name().unwrap().map(|path| path.into_owned()),
            ));
        }

        assert_eq!(
            seen.iter()
                .map(|entry| entry.0.as_str())
                .collect::<Vec<_>>(),
            vec![
                "etc/",
                "etc/config",
                "etc/tool-link",
                "usr/",
                "usr/bin/",
                "usr/bin/tool"
            ]
        );
        assert_eq!(
            (seen[0].2, seen[0].3, seen[0].4, seen[0].5),
            (21, 22, 0o750, 0)
        );
        assert_eq!(seen[1].1, tar::EntryType::Regular);
        assert_eq!(
            (seen[1].2, seen[1].3, seen[1].4, seen[1].5),
            (21, 22, 0o640, 0)
        );
        assert_eq!(seen[1].6, b"config\n");
        assert_eq!(seen[2].1, tar::EntryType::Symlink);
        assert_eq!(
            (seen[2].2, seen[2].3, seen[2].4, seen[2].5),
            (21, 22, 0o777, 0)
        );
        assert_eq!(seen[2].7.as_deref(), Some(Path::new("../usr/bin/tool")));
        assert_eq!(
            (seen[5].2, seen[5].3, seen[5].4, seen[5].5),
            (11, 12, 0o755, 0)
        );
        assert_eq!(seen[5].6, b"tool\n");
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
                object_hash: hash_path(&not_tree).unwrap(),
                object_path: not_tree,
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

    #[test]
    fn tree_merge_logs_stage_counts_and_hash_event() {
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
        let (mut cx, logger) = build_context_with_recording_logger(&temp.path().join("merge"));

        let result = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("right", &right), ("left", &left)]),
                &mut cx,
            )
            .unwrap();
        assert_valid_fs_tree(&result);
        for path in ["bin/left", "etc/right.conf"] {
            assert!(
                fs_tree_manifest(&result)
                    .entries()
                    .iter()
                    .find(|entry| entry.path() == path)
                    .and_then(FsTreeEntry::leaf_hash)
                    .is_some()
            );
        }

        let events = logger.events();
        let phases = events
            .iter()
            .map(|event| event.phase.as_str())
            .collect::<Vec<_>>();
        assert!(phases.contains(&"compose-done"));
        assert!(phases.contains(&"stage-done"));
        assert!(phases.contains(&"ownership-done"));
        assert!(phases.contains(&"hash-done"));

        let stage = events
            .iter()
            .find(|event| event.phase == "stage-done")
            .unwrap();
        assert_eq!(detail_u64(stage, "file_count"), 2);
        assert_eq!(detail_u64(stage, "hardlinked_file_count"), 2);
        assert_eq!(detail_u64(stage, "copied_file_count"), 0);
        assert_eq!(detail_u64(stage, "symlink_count"), 0);
        assert!(detail_u64(stage, "directory_count") >= 3);

        let hash = events
            .iter()
            .find(|event| event.phase == "hash-done")
            .unwrap();
        assert!(hash.object_hash.is_some());
        assert!(hash.details.contains_key("duration_ms"));
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
                object_hash: fixed_object_hash(),
                object_path: base_object,
            },
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let result =
            build_tree_merge(TreeMergeConfig {}, inputs, &mut cx, &FixedHashMaterializer).unwrap();

        assert!(result.object_hash.is_some());
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
                object_hash: hash_path(&base_object).unwrap(),
                object_path: base_object,
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

        assert_eq!(
            result.object_hash,
            Some(hash_path(&result.staged_path).unwrap())
        );
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
    fn tree_merge_allows_identical_duplicate_file_overlap() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "usr/lib64/libsame.so.1".to_string(),
                text: "same\n".to_string(),
                executable: false,
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "usr/lib64/libsame.so.1".to_string(),
                text: "same\n".to_string(),
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

        assert_eq!(
            fs::read_to_string(fs_tree_root(&result).join("usr/lib64/libsame.so.1")).unwrap(),
            "same\n"
        );
        assert_eq!(
            fs_tree_manifest(&result)
                .entries()
                .iter()
                .filter(|entry| entry.path() == "usr/lib64/libsame.so.1")
                .count(),
            1
        );
        assert_valid_fs_tree(&result);
    }

    #[test]
    fn tree_merge_allows_identical_duplicate_symlink_overlap() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::Symlink {
                path: "usr/lib64/libsame.so".to_string(),
                target: "libsame.so.1".to_string(),
            }],
            sample_install(),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::Symlink {
                path: "usr/lib64/libsame.so".to_string(),
                target: "libsame.so.1".to_string(),
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

        assert_eq!(
            fs::read_link(fs_tree_root(&result).join("usr/lib64/libsame.so")).unwrap(),
            Path::new("libsame.so.1")
        );
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
    fn tree_merge_rejects_non_identical_duplicate_files_and_symlinks() {
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
    fn tree_merge_rejects_duplicate_file_with_matching_hash_but_different_metadata() {
        let builder = TreeMergeBuilder;
        let temp = tempdir().unwrap();
        let left = build_fs_tree_for_tests(
            temp.path(),
            "left",
            vec![TreeEntry::File {
                path: "bin/tool".to_string(),
                text: "same\n".to_string(),
                executable: false,
            }],
            install_with_modes(0o755, 0o644),
        );
        let right = build_fs_tree_for_tests(
            temp.path(),
            "right",
            vec![TreeEntry::File {
                path: "bin/tool".to_string(),
                text: "same\n".to_string(),
                executable: false,
            }],
            install_with_modes(0o755, 0o600),
        );
        let mut cx = build_context(&temp.path().join("merge"));

        let error = builder
            .build_typed_for_tests(
                TreeMergeConfig {},
                tree_merge_inputs(&[("left", &left), ("right", &right)]),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("metadata differs"));
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
    fn directory_tree_builds_fs_tree_with_manifest() {
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

        let manifest = fs_tree_manifest(&result);
        assert_eq!(
            manifest.entries(),
            &[
                FsTreeEntry::directory("", 0, 0, 0o755),
                FsTreeEntry::symlink_with_hash(
                    "bin",
                    0,
                    0,
                    "usr/bin",
                    hash_symlink_node(b"usr/bin")
                ),
                FsTreeEntry::directory("dev", 0, 0, 0o755),
                FsTreeEntry::directory("etc", 0, 0, 0o755),
                FsTreeEntry::file_with_hash(
                    "etc/hostname",
                    0,
                    0,
                    0o644,
                    hash_file_bytes(false, b"mbuild\n")
                ),
                FsTreeEntry::file_with_hash(
                    "init",
                    0,
                    0,
                    0o755,
                    hash_file_bytes(true, b"#!/bin/sh\n")
                ),
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
        assert!(manifest.entries().contains(&FsTreeEntry::file_with_hash(
            "etc/hostname",
            42,
            43,
            0o644,
            hash_file_bytes(false, b"mbuild\n")
        )));
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
        assert!(manifest.entries().contains(&FsTreeEntry::file_with_hash(
            "etc/hostname",
            42,
            43,
            0o600,
            hash_file_bytes(false, b"mbuild\n")
        )));
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
                object_hash: fixed_object_hash(),
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
