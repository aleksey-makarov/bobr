mod support;

use mbuild_core::RealizedResult;
use serde_json::json;
use std::fs;
use std::process::{Command, Stdio};
use support::{recipe_node, store_root, text_recipe, write_recipe, write_recipe_with_options};
use tempfile::tempdir;

#[test]
fn cli_reads_recipe_from_stdin_when_path_is_omitted() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("stdin.json");
    write_recipe(
        &recipe_path,
        &text_recipe("stdin-recipe", "hello stdin", false),
    );
    let recipe_bytes = fs::read(&recipe_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.as_mut().unwrap().write_all(&recipe_bytes)?;
            child.wait_with_output()
        })
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _build: RealizedResult = serde_json::from_str(&stdout).unwrap();
    assert!(stderr.contains("[start] Text stdin-recipe"), "{stderr}");
    assert!(stderr.contains("[done] Text stdin-recipe"), "{stderr}");
}

#[test]
fn cli_accepts_explicit_recipe_path() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("custom.json");
    write_recipe(
        &recipe_path,
        &text_recipe("custom-recipe", "hello custom", false),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _build: RealizedResult = serde_json::from_str(&stdout).unwrap();
    assert!(stderr.contains("[start] Text custom-recipe"), "{stderr}");
}

#[test]
fn cli_quiet_suppresses_live_progress() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("quiet.json");
    write_recipe(
        &recipe_path,
        &text_recipe("quiet-recipe", "hello quiet", false),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg("--quiet")
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _build: RealizedResult = serde_json::from_str(&stdout).unwrap();
}

#[test]
fn cli_flags_override_recipe_options() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("override-options.json");
    write_recipe_with_options(
        &recipe_path,
        &text_recipe("override-recipe", "hello override", false),
        &json!({
            "quiet": false,
            "jobs": 0,
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg("--jobs")
        .arg("1")
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("[start] Text override-recipe"), "{stderr}");
}

#[test]
fn recipe_options_apply_when_cli_flags_are_absent() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe-options.json");
    write_recipe_with_options(
        &recipe_path,
        &text_recipe("recipe-quiet", "hello recipe quiet", false),
        &json!({
            "quiet": true,
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn cli_reports_invalid_json_recipe() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("broken.json");
    fs::write(&recipe_path, b"{ not valid json").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("failed to decode recipe JSON value"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_invalid_generic_input_shape() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("broken-shape.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &recipe_path,
        serde_json::to_vec_pretty(&json!({
            "paths": { "store": store.to_string_lossy() },
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

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
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
    let recipe_path = workspace.path().join("relative-store.json");
    fs::write(
        &recipe_path,
        serde_json::to_vec_pretty(&json!({
            "paths": { "store": "relative/store" },
            "nodes": {
                "root": {
                    "name": "text",
                    "tag": "Text",
                    "config": {
                        "source": "hello",
                        "executable": false
                    },
                    "inputs": {}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("$.paths.store: expected absolute path"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_relative_local_path() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("relative-local.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &recipe_path,
        serde_json::to_vec_pretty(&json!({
            "paths": {
                "store": store.to_string_lossy(),
                "local": "relative/local"
            },
            "nodes": {
                "root": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                    "origin": {
                        "type": "path",
                        "path": "payload.txt"
                    },
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("$.paths.local: expected absolute path"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_missing_local_only_when_source_path_materializes() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("missing-local.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &recipe_path,
        serde_json::to_vec_pretty(&json!({
            "paths": { "store": store.to_string_lossy() },
            "nodes": {
                "root": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                    "origin": {
                        "type": "path",
                        "path": "payload.txt"
                    },
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]"), "{stderr}");
    assert!(
        stderr.contains("missing local path base for source origin"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_missing_store_directory() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("missing-store.json");
    let missing_store = workspace.path().join("missing-store");
    fs::write(
        &recipe_path,
        serde_json::to_vec_pretty(&json!({
            "paths": { "store": missing_store.to_string_lossy() },
            "nodes": {
                "root": {
                    "name": "text",
                    "tag": "Text",
                    "config": {
                        "source": "hello",
                        "executable": false
                    },
                    "inputs": {}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("does not exist or is not accessible"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_unknown_input_slot() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("unknown-slot.json");
    let recipe = recipe_node(
        "text",
        "Text",
        json!({
            "source": "hello",
            "executable": false
        }),
        json!({
            "unexpected": text_recipe("dep", "hello", false)
        }),
    );
    write_recipe(&recipe_path, &recipe);

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
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
