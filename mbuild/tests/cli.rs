mod support;

use mbuild_core::Build;
use serde_json::json;
use std::fs;
use std::process::Command;
use support::{recipe_node, text_recipe, write_recipe};
use tempfile::tempdir;

#[test]
fn cli_uses_default_dot_mbuild_recipe_json() {
    let workspace = tempdir().unwrap();
    write_recipe(
        &workspace.path().join(".mbuild").join("recipe.json"),
        &text_recipe("default-recipe", "plain-text", "hello default"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let build: Build = serde_json::from_str(&stdout).unwrap();
    assert_eq!(build.kind, "plain-text");
    assert!(stderr.contains("[start] Text default-recipe"), "{stderr}");
    assert!(stderr.contains("[done] Text default-recipe"), "{stderr}");
}

#[test]
fn cli_accepts_explicit_recipe_path() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("custom.json");
    write_recipe(
        &recipe_path,
        &text_recipe("custom-recipe", "plain-text", "hello custom"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let build: Build = serde_json::from_str(&stdout).unwrap();
    assert_eq!(build.kind, "plain-text");
    assert!(stderr.contains("[start] Text custom-recipe"), "{stderr}");
}

#[test]
fn cli_quiet_suppresses_live_progress() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("quiet.json");
    write_recipe(
        &recipe_path,
        &text_recipe("quiet-recipe", "plain-text", "hello quiet"),
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
    let build: Build = serde_json::from_str(&stdout).unwrap();
    assert_eq!(build.kind, "plain-text");
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
    assert!(stderr.contains("failed to parse recipe JSON"), "{stderr}");
}

#[test]
fn cli_reports_invalid_generic_input_shape() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("broken-shape.json");
    let recipe = recipe_node(
        "bin",
        "Binary",
        json!({
            "kind": "binary-output",
            "optimize": "size"
        }),
        json!({
            "image": [],
            "script": text_recipe("script", "build-script", "#!/bin/sh\nexit 0\n"),
            "sources": []
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
        stderr.contains("input slot 'image' must be a single recipe object"),
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
            "kind": "plain-text",
            "source": "hello"
        }),
        json!({
            "unexpected": null
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
        stderr.contains("does not define input slot 'unexpected'"),
        "{stderr}"
    );
}
