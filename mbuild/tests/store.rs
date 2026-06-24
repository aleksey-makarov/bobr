mod support;

use bobr_store::{Store, load_build_handle};
use mbuild::execute_request;
use std::fs;
use support::{
    build_ref_count, object_record_count, remove_build_ref, store_root, tree_file_recipe,
    write_request,
};
use tempfile::tempdir;

#[test]
fn second_run_reuses_existing_root_build_handle() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("recipe.json");
    let recipe = tree_file_recipe("hello", "hello.txt", "hi\n", false);
    write_request(&request_path, &recipe);

    let first = execute_request(&request_path).unwrap();
    let builds_after_first = build_ref_count(workspace.path());

    let second = execute_request(&request_path).unwrap();
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
fn second_run_reuses_canonical_object_when_build_handle_is_missing() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("recipe.json");
    let recipe = tree_file_recipe("hello", "hello.txt", "hi\n", false);
    write_request(&request_path, &recipe);

    let first = execute_request(&request_path).unwrap();
    let build_key = first.build_key.expect("builder root");
    let object_records_after_first = object_record_count(workspace.path());
    let objects_after_first = fs::read_dir(store_root(workspace.path()).join("objects"))
        .unwrap()
        .count();

    remove_build_ref(workspace.path(), build_key);

    let second = execute_request(&request_path).unwrap();
    let object_records_after_second = object_record_count(workspace.path());
    let objects_after_second = fs::read_dir(store_root(workspace.path()).join("objects"))
        .unwrap()
        .count();
    let builds_after_second = build_ref_count(workspace.path());

    assert_eq!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    assert_eq!(object_records_after_first, 1);
    assert_eq!(object_records_after_second, 1);
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
