#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use mbuild_core::{BuildContext, Builder, BuilderInputs, FsTreeEntry, FsTreeManifest};
use mbuild_runtime::MbuildIdmap;
use mbuild_tree::TreeBuilder;
use serde_json::json;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use tempfile::tempdir;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn tree_directory_output_materializes_runtime_ownership() -> TestResult<()> {
    let idmap = MbuildIdmap::from_host_environment()?;
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

    assert_owner_and_mode(&root, idmap.current_uid(), idmap.current_gid(), 0o755)?;
    assert_owner_and_mode(
        root.join("owned"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o755,
    )?;
    assert_owner_and_mode(
        root.join("owned/dir"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o755,
    )?;
    assert_owner_and_mode(
        root.join("owned/file"),
        idmap.physical_uid(1)?,
        idmap.physical_gid(1)?,
        0o644,
    )?;
    let link = fs::symlink_metadata(root.join("owned/link"))?;
    assert!(link.file_type().is_symlink());
    assert_eq!(link.uid(), idmap.physical_uid(1)?);
    assert_eq!(link.gid(), idmap.physical_gid(1)?);

    mbuild_core::validate_fs_tree_object(&result.staged_path, &idmap)?;

    Ok(())
}

fn assert_owner_and_mode(path: impl AsRef<Path>, uid: u32, gid: u32, mode: u32) -> TestResult<()> {
    let metadata = fs::symlink_metadata(path.as_ref())?;
    assert_eq!(metadata.uid(), uid);
    assert_eq!(metadata.gid(), gid);
    assert_eq!(metadata.permissions().mode() & 0o7777, mode);
    Ok(())
}
