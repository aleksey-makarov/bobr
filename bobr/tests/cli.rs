mod support;

use bobr_core::ObjectHash;
use serde_json::json;
use std::fs;
use std::process::{Command, Stdio};
use support::{
    recipe_node, store_root, tree_file_recipe, write_request, write_request_with_options,
};
use tempfile::tempdir;

#[test]
fn cli_reads_request_from_stdin_when_path_is_omitted() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("stdin.json");
    write_request(
        &request_path,
        &tree_file_recipe("stdin-recipe", "stdin.txt", "hello stdin", false),
    );
    let request_bytes = fs::read(&request_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.as_mut().unwrap().write_all(&request_bytes)?;
            child.wait_with_output()
        })
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _object_hash: ObjectHash = stdout.trim().parse().unwrap();
    assert!(stderr.contains("[start] Tree stdin-recipe"), "{stderr}");
    assert!(stderr.contains("[done] Tree stdin-recipe"), "{stderr}");
}

#[test]
fn cli_accepts_explicit_request_path() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("custom.json");
    write_request(
        &request_path,
        &tree_file_recipe("custom-recipe", "custom.txt", "hello custom", false),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _object_hash: ObjectHash = stdout.trim().parse().unwrap();
    assert!(stderr.contains("[start] Tree custom-recipe"), "{stderr}");
}

#[test]
fn cli_rejects_more_than_one_request_argument() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("one.json");
    let extra_path = workspace.path().join("two.json");
    write_request(
        &request_path,
        &tree_file_recipe("one-recipe", "one.txt", "hello", false),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .arg(&extra_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(stderr.contains("unexpected argument"), "{stderr}");
    assert!(stderr.contains("usage: bobr [request.json]"), "{stderr}");
}

#[test]
fn cli_reports_missing_store_option() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("missing-store-option.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v1",
            "nodes": {
                "root": tree_file_recipe("missing-store-option", "missing.txt", "hello", false)
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(stderr.contains("missing field `store`"), "{stderr}");
}

#[test]
fn request_quiet_suppresses_live_progress() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("quiet.json");
    write_request_with_options(
        &request_path,
        &tree_file_recipe("quiet-recipe", "quiet.txt", "hello quiet", false),
        &json!({
            "quiet": true,
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _object_hash: ObjectHash = stdout.trim().parse().unwrap();
}

#[test]
fn request_jobs_zero_is_rejected() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("zero-jobs.json");
    write_request_with_options(
        &request_path,
        &tree_file_recipe("zero-jobs-recipe", "zero.txt", "hello zero", false),
        &json!({
            "jobs": 0,
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("request 'jobs' must be greater than zero"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_invalid_request() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("broken.json");
    fs::write(&request_path, b"{ not valid json").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("failed to decode request JSON value"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_invalid_generic_input_shape() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("broken-shape.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v1",
            "store": store.to_string_lossy(),
            "nodes": {
                "root": {
                    "name": "sandbox",
                    "tag": "Sandbox",
                    "config": {},
                    "inputs": {
                        "rootfs": []
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(stderr.contains("expected node id string"), "{stderr}");
}

#[test]
fn cli_reports_relative_store_path() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("relative-store.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v1",
            "store": "relative/store",
            "nodes": {
                "root": {
                    "name": "tree",
                    "tag": "Tree",
                    "config": {
                        "tree": {
                            "entries": [{
                                "type": "file",
                                "path": "hello.txt",
                                "text": "hello",
                                "executable": false
                            }]
                        }
                    },
                    "inputs": {}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]"), "{stderr}");
    assert!(stderr.contains("store root must be absolute"), "{stderr}");
}

#[test]
fn cli_reports_unexpected_local_path() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("unexpected-local.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v1",
            "store": store.to_string_lossy(),
            "local": "relative/local",
            "nodes": {
                "root": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                    "origin": {
                        "tag": "Path",
                        "path": "/tmp/payload.txt"
                    },
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown field `local`"), "{stderr}");
}

#[test]
fn cli_reports_relative_source_path() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("relative-source-path.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v1",
            "store": store.to_string_lossy(),
            "nodes": {
                "root": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                    "origin": {
                        "tag": "Path",
                        "path": "payload.txt"
                    },
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("$.nodes.root: origin.path: expected absolute path"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_missing_store_directory() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("missing-store.json");
    let missing_store = workspace.path().join("missing-store");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v1",
            "store": missing_store.to_string_lossy(),
            "nodes": {
                "root": {
                    "name": "tree",
                    "tag": "Tree",
                    "config": {
                        "tree": {
                            "entries": [{
                                "type": "file",
                                "path": "hello.txt",
                                "text": "hello",
                                "executable": false
                            }]
                        }
                    },
                    "inputs": {}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]"), "{stderr}");
    assert!(stderr.contains("store root must exist"), "{stderr}");
}

#[test]
fn cli_reports_unknown_input_slot() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("unknown-slot.json");
    let recipe = recipe_node(
        "tree",
        "Tree",
        json!({
            "tree": {
                "entries": [{
                    "type": "file",
                    "path": "hello.txt",
                    "text": "hello",
                    "executable": false
                }]
            }
        }),
        json!({
            "unexpected": tree_file_recipe("dep", "dep.txt", "hello", false)
        }),
    );
    write_request(&request_path, &recipe);

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("does not accept extra input 'unexpected'"),
        "{stderr}"
    );
}
