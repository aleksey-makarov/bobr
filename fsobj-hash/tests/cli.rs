use fsobj_hash::{hash_path, hash_tar_file, hash_tar_reader};
use std::fs;
use std::io::Cursor;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tar::{Builder, EntryType, Header};
use tempfile::tempdir;

#[test]
fn direct_mode_hashes_regular_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, b"hello direct\n").unwrap();

    let output = run_cli([path.to_str().unwrap(), "--mode=direct"], None);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout_hash(&output), hash_path(&path).unwrap().to_string());
}

#[test]
fn direct_mode_hashes_directory() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("tree")).unwrap();
    fs::write(dir.path().join("tree").join("hello.txt"), b"hello tree\n").unwrap();

    let output = run_cli(
        [dir.path().join("tree").to_str().unwrap(), "--mode=direct"],
        None,
    );

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        stdout_hash(&output),
        hash_path(dir.path().join("tree")).unwrap().to_string()
    );
}

#[test]
fn tar_mode_hashes_tar_file() {
    let dir = tempdir().unwrap();
    let tar_path = dir.path().join("payload.tar");
    let tar_bytes = tar_bytes();
    fs::write(&tar_path, &tar_bytes).unwrap();

    let output = run_cli([tar_path.to_str().unwrap(), "--mode=tar"], None);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        stdout_hash(&output),
        hash_tar_file(&tar_path).unwrap().to_string()
    );
}

#[test]
fn tar_mode_hashes_stdin_tar() {
    let tar_bytes = tar_bytes();

    let output = run_cli(["-", "--mode=tar"], Some(&tar_bytes));

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        stdout_hash(&output),
        hash_tar_reader(Cursor::new(tar_bytes)).unwrap().to_string()
    );
}

#[test]
fn auto_mode_hashes_directory_directly() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("tree")).unwrap();
    fs::write(dir.path().join("tree").join("hello.txt"), b"hello tree\n").unwrap();

    let output = run_cli([dir.path().join("tree").to_str().unwrap()], None);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        stdout_hash(&output),
        hash_path(dir.path().join("tree")).unwrap().to_string()
    );
}

#[test]
fn auto_mode_hashes_regular_non_tar_file_directly() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("payload.txt");
    fs::write(&path, b"plain file\n").unwrap();

    let output = run_cli([path.to_str().unwrap()], None);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout_hash(&output), hash_path(&path).unwrap().to_string());
}

#[test]
fn auto_mode_hashes_tar_suffix_as_tar() {
    let dir = tempdir().unwrap();
    let tar_path = dir.path().join("payload.tar");
    let tar_bytes = tar_bytes();
    fs::write(&tar_path, &tar_bytes).unwrap();

    let output = run_cli([tar_path.to_str().unwrap()], None);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        stdout_hash(&output),
        hash_tar_file(&tar_path).unwrap().to_string()
    );
}

#[test]
fn missing_path_argument_is_usage_error() {
    let output = run_cli::<0>([], None);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("missing path argument"), "{stderr}");
    assert!(stderr.contains("usage: fsobj-hash <path>"), "{stderr}");
}

#[test]
fn invalid_mode_is_rejected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("payload.txt");
    fs::write(&path, b"plain file\n").unwrap();

    let output = run_cli([path.to_str().unwrap(), "--mode=bogus"], None);

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid mode 'bogus'"), "{stderr}");
}

#[test]
fn nonexistent_path_is_rejected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("missing.txt");

    let output = run_cli([path.to_str().unwrap(), "--mode=direct"], None);

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("io error"), "{stderr}");
}

#[test]
fn stdin_without_tar_mode_is_rejected() {
    let output = run_cli(["-"], None);

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("stdin is supported only with '--mode=tar'"),
        "{stderr}"
    );
}

#[test]
fn tar_mode_rejects_non_tar_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("payload.txt");
    fs::write(&path, b"not a tar archive\n").unwrap();

    let output = run_cli([path.to_str().unwrap(), "--mode=tar"], None);

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("tar read error"), "{stderr}");
}

#[test]
fn tar_mode_rejects_directory() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("tree")).unwrap();

    let output = run_cli(
        [dir.path().join("tree").to_str().unwrap(), "--mode=tar"],
        None,
    );

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("tar read error"), "{stderr}");
}

#[test]
fn help_prints_usage_and_exits_successfully() {
    let output = run_cli(["--help"], None);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("usage: fsobj-hash <path>"), "{stdout}");
    assert!(stdout.contains("--mode"), "{stdout}");
    assert!(stdout.contains("auto"), "{stdout}");
    assert!(stdout.contains("direct"), "{stdout}");
    assert!(stdout.contains("tar"), "{stdout}");
    assert!(stdout.contains("'.tar' files as tar archives"), "{stdout}");
}

fn run_cli<const N: usize>(args: [&str; N], stdin: Option<&[u8]>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fsobj-hash"));
    command.args(args);
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(bytes) = stdin {
        child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn stdout_hash(output: &Output) -> String {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .trim()
        .to_string()
}

fn tar_bytes() -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    append_dir(&mut builder, "pkg", 0o755);
    append_file(&mut builder, "pkg/hello.txt", 0o644, b"hello tar\n");
    append_file(&mut builder, "pkg/run.sh", 0o755, b"#!/bin/sh\necho hi\n");
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

fn append_dir(builder: &mut Builder<Vec<u8>>, path: &str, mode: u32) {
    let mut header = Header::new_gnu();
    header.set_size(0);
    header.set_mode(mode);
    header.set_entry_type(EntryType::Directory);
    header.set_cksum();
    builder
        .append_data(&mut header, Path::new(path), std::io::empty())
        .unwrap();
}
