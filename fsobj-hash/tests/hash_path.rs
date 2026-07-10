#![allow(missing_docs)]
use fsobj_hash::{
    DirectoryEntryHash, EntryKind, Error, hash_directory_node, hash_file_bytes,
    hash_fs_tree_object, hash_fs_tree_object_from_hashes, hash_path, hash_path_with_leaf_index,
};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use tempfile::tempdir;

#[test]
fn non_exec_file_hash_equals_plain_sha256() {
    // A non-executable regular file must hash to exactly the SHA-256 of its
    // bytes, so a source can be pinned straight from a published digest.
    //   printf 'payload\n' | sha256sum
    let expected = "d4e4877bac978b7952f0d544fc52ebff5411d351d129f1f056fa43f11da9af2b";
    assert_eq!(hash_file_bytes(false, b"payload\n").to_string(), expected);

    // Hashing a non-executable file on disk goes through the same rule.
    let dir = tempdir().unwrap();
    let path = dir.path().join("payload");
    fs::write(&path, b"payload\n").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(hash_path(&path).unwrap().to_string(), expected);
}

#[test]
fn exec_bit_is_part_of_file_identity() {
    // The executable file keeps a tagged form, so identical bytes with the
    // exec bit set hash differently from the plain-SHA-256 non-exec form.
    let non_exec = hash_file_bytes(false, b"payload\n");
    let exec = hash_file_bytes(true, b"payload\n");
    assert_ne!(non_exec, exec);
}

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

    assert_eq!(
        hash_path(left.path()).unwrap(),
        hash_path(right.path()).unwrap()
    );
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

#[test]
fn synthetic_fs_tree_hash_matches_materialized_object_shape() {
    let dir = tempdir().unwrap();
    let object = dir.path().join("object");
    let root = object.join("root");
    fs::create_dir(&object).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file.txt"), b"hello\n").unwrap();
    let manifest = br#"{"p":"","t":"d","u":0,"g":0,"m":493}
{"p":"file.txt","t":"f","u":0,"g":0,"m":420}
"#;
    fs::write(object.join("manifest.jsonl"), manifest).unwrap();

    assert_eq!(
        hash_fs_tree_object(manifest, &root).unwrap(),
        hash_path(&object).unwrap()
    );
}

#[test]
fn path_hash_with_leaf_index_matches_hash_path() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    fs::create_dir(root.join("bin")).unwrap();
    fs::write(root.join("bin").join("tool"), b"tool\n").unwrap();
    #[cfg(unix)]
    symlink("bin/tool", root.join("tool-link")).unwrap();

    let indexed = hash_path_with_leaf_index(&root).unwrap();
    assert_eq!(indexed.object_hash, hash_path(&root).unwrap());
    assert!(
        indexed
            .leaf_index
            .entries()
            .iter()
            .any(|entry| entry.path == b"bin/tool" && entry.kind == EntryKind::File)
    );
    assert!(
        indexed
            .leaf_index
            .entries()
            .iter()
            .all(|entry| entry.path != b"empty")
    );
}

#[test]
fn directory_hash_from_parts_accounts_for_empty_directories() {
    let empty_dir = hash_directory_node(&[]);
    let file = hash_file_bytes(false, b"payload\n");
    let with_empty_dir = hash_directory_node(&[
        DirectoryEntryHash {
            name: b"empty",
            kind: EntryKind::Directory,
            node_hash: empty_dir,
        },
        DirectoryEntryHash {
            name: b"payload",
            kind: EntryKind::File,
            node_hash: file,
        },
    ]);
    let without_empty_dir = hash_directory_node(&[DirectoryEntryHash {
        name: b"payload",
        kind: EntryKind::File,
        node_hash: file,
    }]);
    assert_ne!(with_empty_dir, without_empty_dir);
}

#[test]
fn fs_tree_object_hash_from_hashes_matches_directory_hash() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("payload"), b"payload\n").unwrap();
    let manifest = br#"{"p":"","t":"d","u":0,"g":0,"m":493}
{"p":"payload","t":"f","u":0,"g":0,"m":420}
"#;
    let root_hash = hash_path(&root).unwrap();
    let manifest_hash = hash_file_bytes(false, manifest);
    assert_eq!(
        hash_fs_tree_object_from_hashes(manifest_hash, root_hash),
        hash_fs_tree_object(manifest, &root).unwrap()
    );
}
