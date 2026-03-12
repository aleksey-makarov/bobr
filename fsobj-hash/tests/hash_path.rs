use fsobj_hash::{Error, hash_path};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use tempfile::tempdir;

#[test]
fn non_exec_mode_changes_do_not_affect_hash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, b"hello").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let first = hash_path(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let second = hash_path(&path).unwrap();
    assert_eq!(first, second);
}

#[test]
fn exec_bit_changes_affect_hash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.sh");
    fs::write(&path, b"echo hi\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let first = hash_path(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let second = hash_path(&path).unwrap();
    assert_ne!(first, second);
}

#[test]
fn empty_directory_hashes() {
    let dir = tempdir().unwrap();
    hash_path(dir.path()).unwrap();
}

#[test]
fn nested_tree_is_path_independent() {
    let left = tempdir().unwrap();
    let right = tempdir().unwrap();

    for root in [left.path(), right.path()] {
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        fs::write(root.join("a/x"), b"one").unwrap();
        fs::write(root.join("b/y"), b"two").unwrap();
    }

    assert_eq!(hash_path(left.path()).unwrap(), hash_path(right.path()).unwrap());
}

#[test]
fn symlink_target_changes_hash() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("tree")).unwrap();
    #[cfg(unix)]
    symlink("lib64", dir.path().join("tree/lib")).unwrap();
    let first = hash_path(dir.path().join("tree")).unwrap();
    fs::remove_file(dir.path().join("tree/lib")).unwrap();
    #[cfg(unix)]
    symlink("usr/lib64", dir.path().join("tree/lib")).unwrap();
    let second = hash_path(dir.path().join("tree")).unwrap();
    assert_ne!(first, second);
}

#[test]
fn mtime_does_not_affect_hash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, b"hello").unwrap();
    let first = hash_path(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, bytes).unwrap();
    let second = hash_path(&path).unwrap();
    assert_eq!(first, second);
}

#[test]
fn root_symlink_is_rejected() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("target"), b"hello").unwrap();
    #[cfg(unix)]
    symlink("target", dir.path().join("root-link")).unwrap();
    let error = hash_path(dir.path().join("root-link")).unwrap_err();
    assert!(matches!(error, Error::UnsupportedRootSymlink { .. }));
}
