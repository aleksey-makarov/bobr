use fsobj_hash::{hash_path, hash_tar_reader};
use std::fs;
use std::io::Cursor;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use tar::{Builder, EntryType, Header};
use tempfile::tempdir;

#[test]
fn empty_directory_matches_empty_tar() {
    let dir = tempdir().unwrap();
    let tar = tar_bytes(|_| {});
    assert_eq!(hash_path(dir.path()).unwrap(), hash_tar_reader(Cursor::new(tar)).unwrap());
}

#[test]
fn nested_directory_matches_tar() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("a")).unwrap();
    fs::create_dir(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/b/c.txt"), b"hello").unwrap();
    fs::write(dir.path().join("root.txt"), b"world").unwrap();

    let tar = tar_bytes(|builder| {
        append_file(builder, "a/b/c.txt", 0o644, b"hello");
        append_file(builder, "root.txt", 0o644, b"world");
    });

    assert_eq!(hash_path(dir.path()).unwrap(), hash_tar_reader(Cursor::new(tar)).unwrap());
}

#[test]
fn symlink_and_exec_bits_match_between_path_and_tar() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    #[cfg(unix)]
    symlink("lib64", dir.path().join("lib")).unwrap();

    let tar = tar_bytes(|builder| {
        append_file(builder, "run.sh", 0o755, b"#!/bin/sh\necho hi\n");
        append_symlink(builder, "lib", "lib64");
    });

    assert_eq!(hash_path(dir.path()).unwrap(), hash_tar_reader(Cursor::new(tar)).unwrap());
}

fn tar_bytes<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut Builder<Vec<u8>>),
{
    let mut builder = Builder::new(Vec::new());
    f(&mut builder);
    builder.finish().unwrap();
    builder.into_inner().unwrap()
}

fn append_file(builder: &mut Builder<Vec<u8>>, path: &str, mode: u32, bytes: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_entry_type(EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes).unwrap();
}

fn append_symlink(builder: &mut Builder<Vec<u8>>, path: &str, target: &str) {
    let mut header = Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o777);
    header.set_entry_type(EntryType::Symlink);
    header.set_link_name(target).unwrap();
    header.set_cksum();
    builder.append_data(&mut header, path, std::io::empty()).unwrap();
}
