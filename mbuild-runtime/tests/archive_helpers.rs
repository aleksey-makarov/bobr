#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use mbuild_core::{FsTreeEntry, FsTreeManifest, InitramfsEntrySource, write_newc_initramfs};
use mbuild_runtime::{
    FsTreeArchiveEntrySource, FsTreeArchiveInput, apply_ownership_batch,
    write_fs_tree_initramfs_in_ownership_namespace, write_fs_tree_tar_in_ownership_namespace,
};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use tempfile::tempdir;
use tracing_subscriber::EnvFilter;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn fs_tree_tar_helper_reads_subuid_owned_file() -> TestResult<()> {
    let _guard = runtime_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    init_tracing();
    let temp = tempdir()?;
    let input_root = temp.path().join("input/root");
    let ownership_workspace = temp.path().join("ownership-workspace");
    let tar_workspace = temp.path().join("tar-workspace");
    let output_tar = temp.path().join("out.tar");
    fs::create_dir_all(&input_root)?;
    fs::create_dir(&ownership_workspace)?;
    fs::create_dir(&tar_workspace)?;
    fs::write(input_root.join("data"), b"payload\n")?;

    let manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::file("data", 1, 1, 0o400),
    ])?;
    apply_ownership_batch(&input_root, &manifest, &ownership_workspace)?;

    write_fs_tree_tar_in_ownership_namespace(
        &[FsTreeArchiveInput {
            root_dir: input_root.clone(),
        }],
        &manifest,
        &[
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::File {
                input_index: 0,
                path: "data".to_string(),
            },
        ],
        &output_tar,
        &tar_workspace,
    )?;

    let entries = read_tar_entries(&output_tar)?;
    assert_eq!(entries, vec![("data".to_string(), b"payload\n".to_vec())]);

    Ok(())
}

#[test]
fn fs_tree_initramfs_helper_matches_core_writer() -> TestResult<()> {
    let _guard = runtime_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    init_tracing();
    let temp = tempdir()?;
    let input_root = temp.path().join("input/root");
    let workspace = temp.path().join("workspace");
    let output_initramfs = temp.path().join("initramfs.img");
    fs::create_dir_all(input_root.join("bin"))?;
    fs::create_dir(&workspace)?;
    fs::write(input_root.join("bin/init"), b"#!/bin/sh\n")?;

    let manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("bin", 0, 0, 0o755),
        FsTreeEntry::file("bin/init", 1, 1, 0o755),
        FsTreeEntry::symlink("bin/sh", 1, 1, "init"),
    ])?;

    write_fs_tree_initramfs_in_ownership_namespace(
        &[FsTreeArchiveInput {
            root_dir: input_root.clone(),
        }],
        &manifest,
        &[
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::Directory,
            FsTreeArchiveEntrySource::File {
                input_index: 0,
                path: "bin/init".to_string(),
            },
            FsTreeArchiveEntrySource::Symlink,
        ],
        &output_initramfs,
        &workspace,
    )?;

    let mut expected = Vec::new();
    write_newc_initramfs(
        &mut expected,
        manifest.entries(),
        &[
            InitramfsEntrySource::Directory,
            InitramfsEntrySource::Directory,
            InitramfsEntrySource::File {
                path: input_root.join("bin/init"),
            },
            InitramfsEntrySource::Symlink,
        ],
    )?;
    assert_eq!(fs::read(&output_initramfs)?, expected);

    Ok(())
}

fn read_tar_entries(path: &Path) -> TestResult<Vec<(String, Vec<u8>)>> {
    let bytes = fs::read(path)?;
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut entries = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let mut contents = Vec::new();
        io::copy(&mut entry, &mut contents)?;
        entries.push((path, contents));
    }
    Ok(entries)
}

fn runtime_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
