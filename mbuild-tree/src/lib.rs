use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputs, BuilderSpec, StagedBuildResult,
    TypedBuilder, fsutil,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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

pub struct TreeBuilder;

static TREE_SPEC: BuilderSpec = BuilderSpec {
    tag: "Tree",
    inputs: &[],
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

        let meta = match output_kind {
            OutputKind::File => {
                materialize_file_output(&output_path, &normalized)?;
                Map::new()
            }
            OutputKind::Directory => {
                materialize_directory_output(&output_path, &normalized)?;
                let install = config
                    .install
                    .expect("validated install for directory output");
                install_meta_map(&install)?
            }
        };

        Ok(StagedBuildResult {
            meta,
            staged_path: output_path,
        })
    }
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
    root: &Path,
    entries: &[NormalizedEntry],
) -> Result<(), BuilderError> {
    fs::create_dir_all(root).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create staged tree root '{}': {error}",
            root.display()
        ))
    })?;

    #[cfg(unix)]
    fs::set_permissions(root, fs::Permissions::from_mode(0o755)).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to set permissions on staged tree root '{}': {error}",
            root.display()
        ))
    })?;

    for entry in entries {
        match entry {
            NormalizedEntry::Dir { rel_path } => {
                let path = root.join(rel_path);
                fs::create_dir_all(&path).map_err(|error| {
                    BuilderError::ExecutionFailed(format!(
                        "failed to create staged directory '{}': {error}",
                        path.display()
                    ))
                })?;
                #[cfg(unix)]
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(|error| {
                    BuilderError::ExecutionFailed(format!(
                        "failed to set permissions on staged directory '{}': {error}",
                        path.display()
                    ))
                })?;
            }
            NormalizedEntry::File {
                rel_path,
                text,
                executable,
            } => {
                let path = root.join(rel_path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to create parent directory '{}': {error}",
                            parent.display()
                        ))
                    })?;
                    #[cfg(unix)]
                    ensure_parent_dirs_0755(root, parent)?;
                }
                fs::write(&path, text).map_err(|error| {
                    BuilderError::ExecutionFailed(format!(
                        "failed to write staged file '{}': {error}",
                        path.display()
                    ))
                })?;
                set_file_mode(&path, *executable)?;
            }
            NormalizedEntry::Symlink { rel_path, target } => {
                let path = root.join(rel_path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to create parent directory '{}': {error}",
                            parent.display()
                        ))
                    })?;
                    #[cfg(unix)]
                    ensure_parent_dirs_0755(root, parent)?;
                }
                create_symlink(target, &path)?;
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
fn ensure_parent_dirs_0755(root: &Path, path: &Path) -> Result<(), BuilderError> {
    let mut current = root.to_path_buf();
    if let Ok(relative) = path.strip_prefix(root) {
        for component in relative.components() {
            if let Component::Normal(segment) = component {
                current.push(segment);
                fs::set_permissions(&current, fs::Permissions::from_mode(0o755)).map_err(
                    |error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to set permissions on staged directory '{}': {error}",
                            current.display()
                        ))
                    },
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_parent_dirs_0755(_root: &Path, _path: &Path) -> Result<(), BuilderError> {
    Ok(())
}

fn set_file_mode(path: &Path, executable: bool) -> Result<(), BuilderError> {
    #[cfg(unix)]
    {
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to set permissions on staged file '{}': {error}",
                path.display()
            ))
        })?;
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

fn install_meta_map(install: &InstallMeta) -> Result<Map<String, Value>, BuilderError> {
    let value = serde_json::to_value(install).map_err(|error| {
        BuilderError::ExecutionFailed(format!("failed to serialize install metadata: {error}"))
    })?;
    let mut meta = Map::new();
    meta.insert("install".to_string(), value);
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{Builder, BuilderInputValue, BuilderInputs};
    use tempfile::tempdir;

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

    #[test]
    fn single_file_tree_builds_file_object() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
        assert!(result.staged_path.join("dev").is_dir());
        assert!(result.meta.get("install").is_some());
    }

    #[test]
    fn materializes_explicit_empty_directories() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
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

        assert!(result.staged_path.join("dev").is_dir());
        assert!(result.staged_path.join("proc").is_dir());
        assert!(result.staged_path.join("sys").is_dir());
        assert_eq!(
            fs::read_dir(result.staged_path.join("dev"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(result.staged_path.join("proc"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(result.staged_path.join("sys"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn materializes_symlink_entries_with_literal_targets() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
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

        let target = fs::read_link(result.staged_path.join("bin")).unwrap();
        assert_eq!(target, PathBuf::from("usr/bin"));
    }

    #[test]
    fn materializes_broken_symlink_entries() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
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

        let target = fs::read_link(result.staged_path.join("etc/mtab")).unwrap();
        assert_eq!(target, PathBuf::from("/proc/self/mounts"));
    }

    #[test]
    fn directory_tree_builds_directory_and_preserves_install_meta() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = builder
            .build_typed(
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
        assert!(result.staged_path.join("dev").is_dir());
        assert_eq!(
            fs::read_to_string(result.staged_path.join("etc/hostname")).unwrap(),
            "mbuild\n"
        );
        assert_eq!(
            fs::read_link(result.staged_path.join("bin")).unwrap(),
            PathBuf::from("usr/bin")
        );
        assert!(result.meta.get("install").is_some());

        #[cfg(unix)]
        {
            let init_mode = fs::metadata(result.staged_path.join("init"))
                .unwrap()
                .permissions()
                .mode();
            let etc_mode = fs::metadata(result.staged_path.join("etc"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(init_mode & 0o777, 0o755);
            assert_eq!(etc_mode & 0o777, 0o755);
        }
    }

    #[test]
    fn file_output_rejects_install_metadata() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = builder
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
    fn tree_builder_rejects_non_empty_inputs() {
        let builder = TreeBuilder;
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        inputs.insert("unexpected", BuilderInputValue::Many(Vec::new()));

        let error = builder
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
            .build_typed(
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
