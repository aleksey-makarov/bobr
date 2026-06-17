use crate::{BuildContext, BuilderInputs, InputSlot, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use mbuild_core::{
    BuildLogLevel, BuilderError, FsTreeEntry, FsTreeManifest, InitramfsEntrySource,
    write_newc_initramfs,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const OUTPUT_FILE_NAME: &str = "initramfs.img";

pub struct InitramfsNewBuilder;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitramfsNewConfig {}

static INITRAMFS_NEW_SPEC: InputSpec = InputSpec {
    required_inputs: &[InputSlot::fs_tree_root("tree")],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for InitramfsNewBuilder {
    type Config = InitramfsNewConfig;

    fn tag(&self) -> &'static str {
        "InitramfsNew"
    }

    fn spec(&self) -> &'static InputSpec {
        &INITRAMFS_NEW_SPEC
    }

    fn build_typed(
        &self,
        _config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        let source_root = inputs.required("tree")?.path.clone();
        let output_path = cx.temp_dir.join(OUTPUT_FILE_NAME);

        cx.log_event(
            BuildLogLevel::Info,
            "initramfs",
            format!(
                "writing deterministic initramfs '{}' from materialized fs-tree root '{}'",
                output_path.display(),
                source_root.display()
            ),
        );

        cx.runtime()
            .run(
                &InitramfsFunction,
                InitramfsInput {
                    source_root,
                    output_path: output_path.clone(),
                },
            )
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;

        Ok(StagedBuildResult {
            staged_path: output_path,
            object_hash: None,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InitramfsFunction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InitramfsInput {
    source_root: PathBuf,
    output_path: PathBuf,
}

impl RuntimeFunction for InitramfsFunction {
    type Input = InitramfsInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "initramfs"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        write_initramfs_image(input).map_err(|error| RuntimeError::new(error.to_string()))
    }
}

fn write_initramfs_image(input: InitramfsInput) -> Result<(), InitramfsError> {
    if input.output_path.exists() {
        return Err(InitramfsError::InvalidInput(format!(
            "InitramfsNew output path already exists: '{}'",
            input.output_path.display()
        )));
    }

    let (manifest, sources) = scan_initramfs_root(&input.source_root)?;
    let output = fs::File::create(&input.output_path).map_err(|error| {
        InitramfsError::Io(format!(
            "failed to create initramfs '{}': {error}",
            input.output_path.display()
        ))
    })?;
    write_newc_initramfs(output, manifest.entries(), &sources)
        .map_err(|error| InitramfsError::Archive(error.to_string()))
}

fn scan_initramfs_root(
    source_root: &Path,
) -> Result<(FsTreeManifest, Vec<InitramfsEntrySource>), InitramfsError> {
    require_existing_real_directory(source_root, "initramfs source root")?;

    let mut entries = Vec::new();
    scan_initramfs_entry(source_root, source_root, &mut entries)?;
    entries.sort_by(|left, right| left.0.path().as_bytes().cmp(right.0.path().as_bytes()));

    let manifest_entries = entries
        .iter()
        .map(|(entry, _source)| entry.clone())
        .collect::<Vec<_>>();
    let manifest = FsTreeManifest::from_entries(manifest_entries)
        .map_err(|error| InitramfsError::InvalidInput(error.to_string()))?;
    let sources = entries
        .into_iter()
        .map(|(_entry, source)| source)
        .collect::<Vec<_>>();

    Ok((manifest, sources))
}

fn scan_initramfs_entry(
    source_root: &Path,
    path: &Path,
    entries: &mut Vec<(FsTreeEntry, InitramfsEntrySource)>,
) -> Result<(), InitramfsError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        InitramfsError::Io(format!(
            "failed to inspect initramfs entry '{}': {error}",
            path.display()
        ))
    })?;
    let file_type = metadata.file_type();
    let rel_path = manifest_relative_path(source_root, path)?;

    if file_type.is_dir() {
        entries.push((
            FsTreeEntry::directory(
                rel_path,
                metadata.uid(),
                metadata.gid(),
                metadata.permissions().mode() & 0o7777,
            ),
            InitramfsEntrySource::Directory,
        ));

        let mut children = fs::read_dir(path)
            .map_err(|error| {
                InitramfsError::Io(format!(
                    "failed to read initramfs directory '{}': {error}",
                    path.display()
                ))
            })?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                InitramfsError::Io(format!(
                    "failed to read initramfs directory entry '{}': {error}",
                    path.display()
                ))
            })?;
        children.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
        for child in children {
            scan_initramfs_entry(source_root, &child, entries)?;
        }
    } else if file_type.is_file() {
        entries.push((
            FsTreeEntry::file(
                rel_path,
                metadata.uid(),
                metadata.gid(),
                metadata.permissions().mode() & 0o7777,
            ),
            InitramfsEntrySource::File {
                path: path.to_path_buf(),
            },
        ));
    } else if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|error| {
            InitramfsError::Io(format!(
                "failed to read initramfs symlink '{}': {error}",
                path.display()
            ))
        })?;
        let target = target.to_str().ok_or_else(|| {
            InitramfsError::InvalidInput(format!(
                "initramfs symlink target for '{}' is not UTF-8",
                path.display()
            ))
        })?;
        entries.push((
            FsTreeEntry::symlink(rel_path, metadata.uid(), metadata.gid(), target),
            InitramfsEntrySource::Symlink,
        ));
    } else {
        return Err(InitramfsError::InvalidInput(format!(
            "unsupported initramfs entry kind at '{}'",
            path.display()
        )));
    }

    Ok(())
}

fn require_existing_real_directory(path: &Path, label: &str) -> Result<(), InitramfsError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        InitramfsError::Io(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(InitramfsError::InvalidInput(format!(
            "{label} must be an existing real directory: '{}'",
            path.display()
        )))
    }
}

fn manifest_relative_path(source_root: &Path, path: &Path) -> Result<String, InitramfsError> {
    let relative = path.strip_prefix(source_root).map_err(|error| {
        InitramfsError::InvalidInput(format!(
            "failed to resolve '{}' relative to '{}': {error}",
            path.display(),
            source_root.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(String::new());
    }
    relative.to_str().map(str::to_string).ok_or_else(|| {
        InitramfsError::InvalidInput(format!(
            "initramfs entry path '{}' is not UTF-8",
            path.display()
        ))
    })
}

#[derive(Debug)]
enum InitramfsError {
    InvalidInput(String),
    Io(String),
    Archive(String),
}

impl std::fmt::Display for InitramfsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(message) | Self::Io(message) | Self::Archive(message) => {
                formatter.write_str(message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Builder, BuilderInputPath};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::tempdir;

    #[test]
    fn input_spec_is_single_fs_tree_root_input() {
        assert_eq!(TypedBuilder::tag(&InitramfsNewBuilder), "InitramfsNew");
        assert_eq!(
            INITRAMFS_NEW_SPEC.required_inputs,
            &[InputSlot::fs_tree_root("tree")]
        );
        assert!(!INITRAMFS_NEW_SPEC.allow_extra_inputs);
    }

    #[test]
    fn build_rejects_missing_tree_input() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = InitramfsNewBuilder
            .build_typed(InitramfsNewConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("required input slot 'tree'"));
    }

    #[test]
    fn runtime_function_writes_deterministic_newc_image() {
        let temp = tempdir().unwrap();
        let root = sample_root(temp.path());
        let first = temp.path().join("first.img");
        let second = temp.path().join("second.img");

        write_initramfs_image(InitramfsInput {
            source_root: root.clone(),
            output_path: first.clone(),
        })
        .unwrap();
        write_initramfs_image(InitramfsInput {
            source_root: root,
            output_path: second.clone(),
        })
        .unwrap();

        assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
    }

    #[test]
    fn runtime_function_records_file_directory_and_symlink_entries() {
        let temp = tempdir().unwrap();
        let root = sample_root(temp.path());

        let (manifest, sources) = scan_initramfs_root(&root).unwrap();
        let paths = manifest
            .entries()
            .iter()
            .map(FsTreeEntry::path)
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["", "bin", "bin/tool", "tool-link"]);
        assert!(matches!(sources[0], InitramfsEntrySource::Directory));
        assert!(matches!(sources[1], InitramfsEntrySource::Directory));
        assert!(matches!(sources[2], InitramfsEntrySource::File { .. }));
        assert!(matches!(sources[3], InitramfsEntrySource::Symlink));
    }

    #[test]
    fn runtime_function_rejects_existing_output_path() {
        let temp = tempdir().unwrap();
        let root = sample_root(temp.path());
        let output_path = temp.path().join("initramfs.img");
        fs::write(&output_path, b"already exists").unwrap();

        let error = write_initramfs_image(InitramfsInput {
            source_root: root,
            output_path,
        })
        .unwrap_err();

        assert!(error.to_string().contains("output path already exists"));
    }

    #[test]
    fn build_rejects_unknown_config_field() {
        let error = InitramfsNewBuilder
            .build_erased(
                serde_json::json!({"extra": true}),
                BuilderInputs::empty(),
                &mut BuildContext::with_noop_logger(PathBuf::from("/tmp/unused")),
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn builder_accepts_tree_input_path_shape() {
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "tree",
            BuilderInputPath {
                path: PathBuf::from("/fs-tree-root"),
            },
        );

        assert_eq!(
            inputs.required("tree").unwrap().path,
            PathBuf::from("/fs-tree-root")
        );
    }

    fn sample_root(parent: &Path) -> PathBuf {
        let root = parent.join("root");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::set_permissions(root.join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("bin/tool"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("bin/tool", root.join("tool-link")).unwrap();
        root
    }
}
