#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use mbuild_core::{
    BuildContext, Builder, BuilderInputObject, BuilderInputs, FsTreeEntry, FsTreeManifest,
};
use mbuild_runtime::{
    FsTreeArchiveEntrySource, FsTreeArchiveInput, write_fs_tree_tar_in_ownership_namespace,
};
use mbuild_tree::{TreeBuilder, TreeMergeBuilder, TreeSubsetBuilder};
use serde_json::json;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use tempfile::tempdir;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn tree_directory_output_materializes_runtime_ownership() -> TestResult<()> {
    let temp = tempdir()?;
    let state_dir = temp.path().join("state");
    let temp_dir = state_dir.join("tmp");
    fs::create_dir_all(&state_dir)?;
    fs::create_dir(&temp_dir)?;
    let mut cx = BuildContext::with_noop_logger(state_dir, temp_dir);

    let result = TreeBuilder.build_erased(
        json!({
            "tree": {
                "entries": [
                    { "type": "dir", "path": "owned/dir" },
                    {
                        "type": "file",
                        "path": "owned/file",
                        "text": "owned\n",
                        "executable": false
                    },
                    {
                        "type": "symlink",
                        "path": "owned/link",
                        "target": "file"
                    }
                ]
            },
            "install": {
                "rules": [
                    {
                        "path": "**",
                        "attrs": {
                            "uid": 0,
                            "gid": 0,
                            "directory_mode": 493,
                            "regular_file_mode": 420,
                            "executable_file_mode": 493,
                            "symlink_mode": 511
                        }
                    },
                    {
                        "path": "owned/**",
                        "attrs": {
                            "uid": 1,
                            "gid": 1,
                            "directory_mode": 493,
                            "regular_file_mode": 420,
                            "executable_file_mode": 448,
                            "symlink_mode": 511
                        }
                    }
                ]
            }
        }),
        BuilderInputs::empty(),
        &mut cx,
    )?;

    let root = result.staged_path.join("root");
    let file_hash = fsobj_hash::hash_path(root.join("owned/file"))?;
    let symlink_hash = fsobj_hash::hash_symlink_node(b"file");
    let manifest = FsTreeManifest::read_canonical(&result.staged_path.join("manifest.jsonl"))?;
    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::directory("owned", 1, 1, 0o755))
    );
    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::directory("owned/dir", 1, 1, 0o755))
    );
    assert!(manifest.entries().iter().any(|entry| {
        matches!(
            entry,
            FsTreeEntry::File {
                path,
                uid: 1,
                gid: 1,
                mode: 0o644,
                hash,
            } if path == "owned/file" && *hash == file_hash
        )
    }));
    assert!(manifest.entries().iter().any(|entry| {
        matches!(
            entry,
            FsTreeEntry::Symlink {
                path,
                uid: 1,
                gid: 1,
                target,
                hash,
            } if path == "owned/link" && target == "file" && *hash == symlink_hash
        )
    }));

    let root_metadata = assert_mode_and_read_owner(&root, 0o755)?;
    let owned_metadata = assert_mode_and_read_owner(root.join("owned"), 0o755)?;
    let dir_metadata = assert_mode_and_read_owner(root.join("owned/dir"), 0o755)?;
    let file_metadata = assert_mode_and_read_owner(root.join("owned/file"), 0o644)?;
    assert_eq!(dir_metadata.uid(), owned_metadata.uid());
    assert_eq!(dir_metadata.gid(), owned_metadata.gid());
    assert_eq!(file_metadata.uid(), owned_metadata.uid());
    assert_eq!(file_metadata.gid(), owned_metadata.gid());
    assert_ne!(owned_metadata.uid(), root_metadata.uid());
    assert_ne!(owned_metadata.gid(), root_metadata.gid());
    let link = fs::symlink_metadata(root.join("owned/link"))?;
    assert!(link.file_type().is_symlink());
    assert_eq!(link.uid(), owned_metadata.uid());
    assert_eq!(link.gid(), owned_metadata.gid());

    Ok(())
}

#[test]
fn tree_subset_materializes_subuid_owned_file_under_restrictive_dir() -> TestResult<()> {
    let temp = tempdir()?;
    let input = build_restrictive_tree(temp.path(), "input")?;
    let state_dir = temp.path().join("subset-state");
    let temp_dir = state_dir.join("tmp");
    fs::create_dir_all(&state_dir)?;
    fs::create_dir(&temp_dir)?;
    let mut cx = BuildContext::with_noop_logger(state_dir, temp_dir);

    let result = TreeSubsetBuilder.build_erased(
        json!({ "include": ["owned/file"] }),
        single_tree_input("tree", &input),
        &mut cx,
    )?;

    assert_file_contents_via_tar(temp.path(), &result, "owned/file", b"owned\n")?;
    assert_mode_and_read_owner(result.staged_path.join("root/owned"), 0o500)?;
    assert_manifest_file_mode(&result, "owned/file", 0o400)?;

    Ok(())
}

#[test]
fn tree_merge_materializes_subuid_owned_file_under_restrictive_dir() -> TestResult<()> {
    let temp = tempdir()?;
    let left = build_restrictive_tree(temp.path(), "left")?;
    let right = build_simple_tree(temp.path(), "right")?;
    let state_dir = temp.path().join("merge-state");
    let temp_dir = state_dir.join("tmp");
    fs::create_dir_all(&state_dir)?;
    fs::create_dir(&temp_dir)?;
    let mut cx = BuildContext::with_noop_logger(state_dir, temp_dir);
    let mut inputs = single_tree_input("left", &left);
    inputs.insert(
        "right",
        BuilderInputObject {
            path: right.staged_path.clone(),
        },
    );

    let result = TreeMergeBuilder.build_erased(json!({}), inputs, &mut cx)?;

    assert_file_contents_via_tar(temp.path(), &result, "owned/file", b"owned\n")?;
    assert_eq!(
        fs::read_to_string(result.staged_path.join("root/etc/right.conf"))?,
        "right\n"
    );
    assert_mode_and_read_owner(result.staged_path.join("root/owned"), 0o500)?;
    assert_manifest_file_mode(&result, "owned/file", 0o400)?;

    Ok(())
}

fn build_restrictive_tree(root: &Path, name: &str) -> TestResult<mbuild_core::StagedBuildResult> {
    build_tree(
        root,
        name,
        json!({
            "tree": {
                "entries": [
                    {
                        "type": "file",
                        "path": "owned/file",
                        "text": "owned\n",
                        "executable": false
                    }
                ]
            },
            "install": {
                "rules": [
                    {
                        "path": "**",
                        "attrs": {
                            "uid": 0,
                            "gid": 0,
                            "directory_mode": 493,
                            "regular_file_mode": 420,
                            "executable_file_mode": 493,
                            "symlink_mode": 511
                        }
                    },
                    {
                        "path": "owned/**",
                        "attrs": {
                            "uid": 1,
                            "gid": 1,
                            "directory_mode": 320,
                            "regular_file_mode": 256,
                            "executable_file_mode": 320,
                            "symlink_mode": 511
                        }
                    }
                ]
            }
        }),
    )
}

fn build_simple_tree(root: &Path, name: &str) -> TestResult<mbuild_core::StagedBuildResult> {
    build_tree(
        root,
        name,
        json!({
            "tree": {
                "entries": [
                    {
                        "type": "file",
                        "path": "etc/right.conf",
                        "text": "right\n",
                        "executable": false
                    }
                ]
            },
            "install": {
                "rules": [
                    {
                        "path": "**",
                        "attrs": {
                            "uid": 0,
                            "gid": 0,
                            "directory_mode": 493,
                            "regular_file_mode": 420,
                            "executable_file_mode": 493,
                            "symlink_mode": 511
                        }
                    }
                ]
            }
        }),
    )
}

fn build_tree(
    root: &Path,
    name: &str,
    config: serde_json::Value,
) -> TestResult<mbuild_core::StagedBuildResult> {
    let state_dir = root.join(format!("{name}-state"));
    let temp_dir = state_dir.join("tmp");
    fs::create_dir_all(&state_dir)?;
    fs::create_dir(&temp_dir)?;
    let mut cx = BuildContext::with_noop_logger(state_dir, temp_dir);
    Ok(TreeBuilder.build_erased(config, BuilderInputs::empty(), &mut cx)?)
}

fn single_tree_input(name: &'static str, result: &mbuild_core::StagedBuildResult) -> BuilderInputs {
    let mut inputs = BuilderInputs::empty();
    inputs.insert(
        name,
        BuilderInputObject {
            path: result.staged_path.clone(),
        },
    );
    inputs
}

fn assert_file_contents_via_tar(
    root: &Path,
    result: &mbuild_core::StagedBuildResult,
    rel_path: &str,
    expected: &[u8],
) -> TestResult<()> {
    let manifest = FsTreeManifest::read_canonical(&result.staged_path.join("manifest.jsonl"))?;
    let sources = manifest
        .entries()
        .iter()
        .map(|entry| match entry {
            FsTreeEntry::Directory { .. } => FsTreeArchiveEntrySource::Directory,
            FsTreeEntry::File { path, .. } => FsTreeArchiveEntrySource::File {
                input_index: 0,
                path: path.clone(),
            },
            FsTreeEntry::Symlink { .. } => FsTreeArchiveEntrySource::Symlink,
        })
        .collect::<Vec<_>>();
    let workspace = root.join("tar-workspace");
    let output_tar = root.join("tree-output.tar");
    fs::create_dir(&workspace)?;
    write_fs_tree_tar_in_ownership_namespace(
        &[FsTreeArchiveInput {
            root_dir: result.staged_path.join("root"),
        }],
        &manifest,
        &sources,
        &output_tar,
        &workspace,
    )?;

    let bytes = fs::read(output_tar)?;
    let mut archive = tar::Archive::new(bytes.as_slice());
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == Path::new(rel_path) {
            let mut contents = Vec::new();
            io::copy(&mut entry, &mut contents)?;
            assert_eq!(contents, expected);
            return Ok(());
        }
    }
    Err(format!("tar output did not contain '{rel_path}'").into())
}

fn assert_manifest_file_mode(
    result: &mbuild_core::StagedBuildResult,
    rel_path: &str,
    mode: u32,
) -> TestResult<()> {
    let manifest = FsTreeManifest::read_canonical(&result.staged_path.join("manifest.jsonl"))?;
    assert!(manifest.entries().iter().any(|entry| {
        matches!(
            entry,
            FsTreeEntry::File {
                path,
                mode: entry_mode,
                ..
            } if path == rel_path && *entry_mode == mode
        )
    }));
    Ok(())
}

fn assert_mode_and_read_owner(path: impl AsRef<Path>, mode: u32) -> TestResult<fs::Metadata> {
    let metadata = fs::symlink_metadata(path.as_ref())?;
    assert_eq!(metadata.permissions().mode() & 0o7777, mode);
    Ok(metadata)
}
