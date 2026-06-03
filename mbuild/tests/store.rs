mod support;

use mbuild::recipe_runtime::run_recipe_json_in_workspace;
use mbuild_store::{Store, load_build_handle};
use std::fs;
use support::{
    build_ref_count, remove_build_ref, result_record_count, store_root, tree_file_recipe,
    write_recipe,
};
use tempfile::tempdir;

#[test]
fn second_run_reuses_existing_root_build_handle() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe.json");
    let recipe = tree_file_recipe("hello", "hello.txt", "hi\n", false);
    write_recipe(&recipe_path, &recipe);

    let first = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let builds_after_first = build_ref_count(workspace.path());

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let builds_after_second = build_ref_count(workspace.path());

    assert_eq!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    assert!(
        load_build_handle(
            &Store::create(&store_root(workspace.path())).unwrap(),
            first.build_key.expect("builder root"),
        )
        .unwrap()
        .is_some()
    );
    assert_eq!(builds_after_first, 1);
    assert_eq!(builds_after_second, 1);
}

#[test]
fn second_run_reuses_canonical_result_when_build_handle_is_missing() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe.json");
    let recipe = tree_file_recipe("hello", "hello.txt", "hi\n", false);
    write_recipe(&recipe_path, &recipe);

    let first = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let build_key = first.build_key.expect("builder root");
    let results_after_first = result_record_count(workspace.path());
    let objects_after_first = fs::read_dir(store_root(workspace.path()).join("objects"))
        .unwrap()
        .count();

    remove_build_ref(workspace.path(), build_key);

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let results_after_second = result_record_count(workspace.path());
    let objects_after_second = fs::read_dir(store_root(workspace.path()).join("objects"))
        .unwrap()
        .count();
    let builds_after_second = build_ref_count(workspace.path());

    assert_eq!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    assert_eq!(results_after_first, 1);
    assert_eq!(results_after_second, 1);
    assert_eq!(objects_after_first, 1);
    assert_eq!(objects_after_second, 1);
    assert_eq!(builds_after_second, 1);
    assert!(
        load_build_handle(
            &Store::create(&store_root(workspace.path())).unwrap(),
            build_key
        )
        .unwrap()
        .is_some()
    );
}
