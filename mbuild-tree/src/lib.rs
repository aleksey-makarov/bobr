use fsobj_hash::{ObjectHash, hash_path};
use globset::{Glob, GlobMatcher};
use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputs, BuilderSpec, FsTreeEntry,
    FsTreeManifest, StagedBuildResult, TypedBuilder, create_fs_tree_staging_dir, fsutil,
};
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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

#[derive(Debug, Clone, Copy)]
enum MaterializedKind {
    File { executable: bool },
    Directory,
    Symlink,
}

trait OwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        object_dir: &Path,
        manifest: &FsTreeManifest,
        temp_dir: &Path,
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
}

pub struct TreeBuilder;

static TREE_SPEC: BuilderSpec = BuilderSpec {
    tag: "Tree",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: false,
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
            NormalizedEntry::Symlink { rel_path, .. } => {
                fs_tree_entry_for_path(rel_path, MaterializedKind::Symlink, rules)?
            }
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
        MaterializedKind::Symlink => Ok(FsTreeEntry::symlink(rel_path, uid, gid)),
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
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    #[derive(Debug, Clone, Copy)]
    struct CurrentOwnerMaterializer;

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
                FsTreeEntry::symlink("bin", 0, 0),
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
