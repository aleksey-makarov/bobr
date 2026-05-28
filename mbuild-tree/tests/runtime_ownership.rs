#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use mbuild_core::{BuildContext, Builder, BuilderInputs, FsTreeEntry, FsTreeManifest};
use mbuild_tree::TreeBuilder;
use serde_json::json;
use std::fs;
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

fn assert_mode_and_read_owner(path: impl AsRef<Path>, mode: u32) -> TestResult<fs::Metadata> {
    let metadata = fs::symlink_metadata(path.as_ref())?;
    assert_eq!(metadata.permissions().mode() & 0o7777, mode);
    Ok(metadata)
}
