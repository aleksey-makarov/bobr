mod support;

use support::{build_ref_path, text_recipe, write_recipe};
use mbuild::recipe_runtime::run_recipe_json_in_workspace;
use std::fs;
use tempfile::tempdir;

#[test]
fn second_run_reuses_existing_root_build_handle() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe.json");
    let recipe = text_recipe("hello", "plain-text", "hi\n");
    write_recipe(&recipe_path, &recipe);

    let first = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let builds_after_first = fs::read_dir(workspace.path().join(".mbuild").join("builds"))
        .unwrap()
        .count();

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let builds_after_second = fs::read_dir(workspace.path().join(".mbuild").join("builds"))
        .unwrap()
        .count();

    assert_eq!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    assert_eq!(builds_after_first, 1);
    assert_eq!(builds_after_second, 1);
}

#[test]
fn second_run_reuses_canonical_result_when_build_handle_is_missing() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe.json");
    let recipe = text_recipe("hello", "plain-text", "hi\n");
    write_recipe(&recipe_path, &recipe);

    let first = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let build_ref = build_ref_path(workspace.path(), first.build_key);
    let results_after_first = fs::read_dir(workspace.path().join(".mbuild").join("results"))
        .unwrap()
        .count();
    let objects_after_first = fs::read_dir(workspace.path().join(".mbuild").join("objects"))
        .unwrap()
        .count();

    fs::remove_file(&build_ref).unwrap();
    assert!(!build_ref.exists());

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let results_after_second = fs::read_dir(workspace.path().join(".mbuild").join("results"))
        .unwrap()
        .count();
    let objects_after_second = fs::read_dir(workspace.path().join(".mbuild").join("objects"))
        .unwrap()
        .count();
    let builds_after_second = fs::read_dir(workspace.path().join(".mbuild").join("builds"))
        .unwrap()
        .count();

    assert_eq!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    assert_eq!(results_after_first, 1);
    assert_eq!(results_after_second, 1);
    assert_eq!(objects_after_first, 1);
    assert_eq!(objects_after_second, 1);
    assert_eq!(builds_after_second, 1);
    assert!(build_ref.exists());
}
