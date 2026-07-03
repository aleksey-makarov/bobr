use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_core::BuildLogLevel;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Configuration for [`TreeBuilder`]: an inline `tree` of files, directories,
/// and symlinks.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    tree: TreePayload,
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

/// Builds a content tree from an inline `tree` description (takes no inputs).
#[derive(Debug)]
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

    fn impl_version(&self) -> &'static str {
        "1"
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
        build_tree(config, inputs, cx)
    }
}

fn build_tree(
    config: TreeConfig,
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
    let output_path = cx
        .temp_dir
        .join(format!("tree-{}.obj", current_epoch_nanos()?));

    cx.log_event(
        BuildLogLevel::Info,
        "stage",
        format!("materializing tree output '{}'", output_path.display()),
    );

    match output_kind {
        OutputKind::File => materialize_file_output(&output_path, &normalized)?,
        OutputKind::Directory => materialize_directory_output(&output_path, &normalized)?,
    }

    Ok(StagedBuildResult {
        staged_path: output_path,
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
    if entries.len() == 1
        && let Some(NormalizedEntry::File { rel_path, .. }) = entries.first()
        && !rel_path.contains('/')
    {
        return OutputKind::File;
    }
    OutputKind::Directory
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
    let Some(NormalizedEntry::File {
        text, executable, ..
    }) = entries.first()
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: file output requires one file entry".to_string(),
        ));
    };

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
    output_dir: &Path,
    entries: &[NormalizedEntry],
) -> Result<(), BuilderError> {
    fs::create_dir(output_dir).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create staged directory '{}': {error}",
            output_dir.display()
        ))
    })?;

    let mut directories = BTreeSet::from([String::new()]);
    for entry in entries {
        add_parent_directories(entry.rel_path(), &mut directories);
        if let NormalizedEntry::Dir { rel_path } = entry {
            directories.insert(rel_path.clone());
        }
    }

    let mut sorted_dirs = directories
        .iter()
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    sorted_dirs.sort_by_key(|path| path.split('/').count());
    for rel_path in sorted_dirs {
        let path = output_dir.join(rel_path);
        fs::create_dir(&path).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to create staged directory '{}': {error}",
                path.display()
            ))
        })?;
    }

    for entry in entries {
        match entry {
            NormalizedEntry::Dir { .. } => {}
            NormalizedEntry::File {
                rel_path,
                text,
                executable,
            } => {
                let path = output_dir.join(rel_path);
                fs::write(&path, text).map_err(|error| {
                    BuilderError::ExecutionFailed(format!(
                        "failed to write staged file '{}': {error}",
                        path.display()
                    ))
                })?;
                set_file_mode(&path, *executable)?;
            }
            NormalizedEntry::Symlink { rel_path, target } => {
                let path = output_dir.join(rel_path);
                symlink(target, &path).map_err(|error| {
                    BuilderError::ExecutionFailed(format!(
                        "failed to create symlink '{}' -> '{}': {error}",
                        path.display(),
                        target
                    ))
                })?;
            }
        }
    }

    set_directory_modes_post_order(output_dir, &directories)
}

fn add_parent_directories(rel_path: &str, directories: &mut BTreeSet<String>) {
    let mut current = PathBuf::new();
    let components = rel_path.split('/').collect::<Vec<_>>();
    for segment in components.iter().take(components.len().saturating_sub(1)) {
        current.push(segment);
        directories.insert(current.to_string_lossy().replace('\\', "/"));
    }
}

fn set_directory_modes_post_order(
    output_dir: &Path,
    directories: &BTreeSet<String>,
) -> Result<(), BuilderError> {
    let mut dirs = directories.iter().collect::<Vec<_>>();
    dirs.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });

    for rel_path in dirs {
        let path = if rel_path.is_empty() {
            output_dir.to_path_buf()
        } else {
            output_dir.join(rel_path)
        };
        set_mode(&path, 0o755)?;
    }

    Ok(())
}

fn set_file_mode(path: &Path, executable: bool) -> Result<(), BuilderError> {
    let mode = if executable { 0o755 } else { 0o644 };
    set_mode(path, mode)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), BuilderError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to set mode {:o} on '{}': {error}",
            mode,
            path.display()
        ))
    })
}

fn current_epoch_nanos() -> Result<u128, BuilderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!("system time before UNIX_EPOCH: {error}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Builder;
    use crate::test_support::store_fs_tree;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn build_context(root: &Path) -> BuildContext {
        let temp_dir = root.join("tmp");
        fs::create_dir(&temp_dir).unwrap();
        BuildContext::with_noop_logger(temp_dir.clone(), store_fs_tree(root))
    }

    #[test]
    fn file_output_stages_plain_file() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = TreeBuilder
            .build_typed(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![TreeEntry::File {
                            path: "tool".to_string(),
                            text: "hello\n".to_string(),
                            executable: true,
                        }],
                    },
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert_eq!(fs::read_to_string(&result.staged_path).unwrap(), "hello\n");
        assert_eq!(
            fs::metadata(&result.staged_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
        );
    }

    #[test]
    fn directory_output_stages_plain_directory_without_manifest_layout() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let result = TreeBuilder
            .build_typed(
                TreeConfig {
                    tree: TreePayload {
                        entries: vec![
                            TreeEntry::Dir {
                                path: "etc".to_string(),
                            },
                            TreeEntry::File {
                                path: "etc/hostname".to_string(),
                                text: "host\n".to_string(),
                                executable: false,
                            },
                            TreeEntry::Symlink {
                                path: "bin".to_string(),
                                target: "usr/bin".to_string(),
                            },
                        ],
                    },
                },
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap();

        assert!(result.staged_path.is_dir());
        assert!(!result.staged_path.join("manifest.jsonl").exists());
        assert!(!result.staged_path.join("root").exists());
        assert_eq!(
            fs::read_to_string(result.staged_path.join("etc/hostname")).unwrap(),
            "host\n",
        );
        assert_eq!(
            fs::read_link(result.staged_path.join("bin")).unwrap(),
            PathBuf::from("usr/bin"),
        );
        assert_eq!(
            fs::metadata(result.staged_path.join("etc/hostname"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644,
        );
    }

    #[test]
    fn install_field_is_rejected() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());

        let error = TreeBuilder
            .build_erased(
                serde_json::json!({
                    "tree": {
                        "entries": [{
                            "type": "file",
                            "path": "tool",
                            "text": "hello",
                            "executable": false
                        }]
                    },
                    "install": {"rules": []}
                }),
                BuilderInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown field `install`"));
    }
}
