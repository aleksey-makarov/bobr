use super::install::{
    CompiledInstallRule, InstallMeta, compile_install_rules, resolve_install_attrs,
};
use super::legacy_object::{
    OwnershipMaterializer, RuntimeOwnershipMaterializer, create_symlink, current_epoch_nanos,
    map_fs_tree_error, set_file_mode, set_mode,
};
use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder};
use fsobj_hash::{ObjectHash, hash_file_bytes, hash_path, hash_symlink_node};
use mbuild_core::{
    BuildLogLevel, BuilderError, FsTreeEntry, FsTreeManifest, create_fs_tree_staging_dir,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    pub(super) tree: TreePayload,
    #[serde(default)]
    pub(super) install: Option<InstallMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TreePayload {
    pub(super) entries: Vec<TreeEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum TreeEntry {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NormalizedEntry {
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
pub(super) enum OutputKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
pub(super) enum MaterializedKind {
    File { text: String, executable: bool },
    Directory,
    Symlink { target: String },
}

pub struct TreeBuilder;

static TREE_SPEC: InputSpec = InputSpec {
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for TreeBuilder {
    type Config = TreeConfig;

    fn tag(&self) -> &'static str {
        "Tree"
    }

    fn spec(&self) -> &'static InputSpec {
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

pub(super) fn build_tree(
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

    let now_nanos = current_epoch_nanos()?;
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

pub(super) fn normalize_entries(
    entries: Vec<TreeEntry>,
) -> Result<Vec<NormalizedEntry>, BuilderError> {
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

pub(super) fn determine_output_kind(entries: &[NormalizedEntry]) -> OutputKind {
    if entries.len() == 1
        && let Some(NormalizedEntry::File { rel_path, .. }) = entries.first()
        && !rel_path.contains('/')
    {
        return OutputKind::File;
    }
    OutputKind::Directory
}

pub(super) fn validate_install(
    kind: OutputKind,
    install: Option<&InstallMeta>,
) -> Result<(), BuilderError> {
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

pub(super) fn validate_rel_path(path: &str) -> Result<String, BuilderError> {
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

pub(super) fn materialize_file_output(
    path: &Path,
    entries: &[NormalizedEntry],
) -> Result<(), BuilderError> {
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

pub(super) fn materialize_directory_output(
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

pub(super) fn apply_directory_modes_post_order(
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

pub(super) fn fs_tree_manifest_for_entries(
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

pub(super) fn add_parent_directories(
    rel_path: &str,
    manifest_entries: &mut BTreeMap<String, FsTreeEntry>,
    rules: &[CompiledInstallRule],
) -> Result<(), BuilderError> {
    let mut current = PathBuf::new();
    let components = rel_path.split('/').collect::<Vec<_>>();
    for segment in components.iter().take(components.len().saturating_sub(1)) {
        current.push(segment);
        let path = current.to_string_lossy().replace('\\', "/");
        if let std::collections::btree_map::Entry::Vacant(entry_slot) = manifest_entries.entry(path)
        {
            let path = entry_slot.key().to_string();
            let entry = fs_tree_entry_for_path(&path, MaterializedKind::Directory, rules)?;
            entry_slot.insert(entry);
        }
    }
    Ok(())
}

pub(super) fn fs_tree_entry_for_path(
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

pub(super) fn required_attr(
    value: Option<u32>,
    rel_path: &str,
    name: &str,
) -> Result<u32, BuilderError> {
    value.ok_or_else(|| {
        BuilderError::InvalidRecipe(format!(
            "invalid builder config: path '{rel_path}' is missing resolved {name}"
        ))
    })
}
