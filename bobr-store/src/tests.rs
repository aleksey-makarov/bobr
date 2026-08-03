use super::*;
use crate::fs_tree::{FsTree, FsTreeEntry, FsTreeInstall, FsTreeInstallAttrs, FsTreeInstallRule};
use bobr_core::{
    BuildKey, ConfigDigest, ObjectHash, ReuseKey, compute_build_key, compute_reuse_key,
};
use fsobj_hash::hash_path;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tempfile::tempdir;

fn create_test_store(root: &Path) -> Store {
    let store_root = root.join(".bobr");
    fs::create_dir_all(&store_root).unwrap();
    Store::create(&store_root).unwrap()
}

/// Stands in for the run id a driver would stamp into object records.
const TEST_RUN_ID: &str = "test-run";

fn fs_files_dir(store: &Store) -> PathBuf {
    store.root().join(store::FS_FILES_DIR)
}

fn fs_trees_dir(store: &Store) -> PathBuf {
    store.root().join(store::FS_TREES_DIR)
}

fn fs_tree_refs_dir(store: &Store) -> PathBuf {
    store.root().join(store::FS_TREE_REFS_DIR)
}

#[derive(Debug, Clone, Copy)]
struct TestBuild {
    object_hash: ObjectHash,
    build_key: BuildKey,
}

#[test]
fn reuse_key_is_stable_for_identical_inputs() {
    let payload = json!({ "kind": "sandbox-script" });
    let inputs = vec![
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111"),
        parse_object_hash("2222222222222222222222222222222222222222222222222222222222222222"),
    ];

    assert_eq!(
        reuse_key_for("CasTest", payload.clone(), &inputs),
        reuse_key_for("CasTest", payload, &inputs)
    );
}

#[test]
fn source_build_key_uses_object_hash_bytes() {
    let hex = "1111111111111111111111111111111111111111111111111111111111111111";
    let object_hash = parse_object_hash(hex);

    assert_eq!(BuildKey::from_object_hash(object_hash).to_string(), hex);
}

#[test]
fn parse_object_record_rejects_old_schema() {
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    let value = json!({
        "schema": "bobr-object-record-v1",
        "object_hash": object_hash.to_string(),
        "inputs": [],
    });

    assert!(matches!(
        parse_object_record_value(object_hash, &value),
        Err(StoreError::InvalidData(message))
            if message == "unsupported object record schema 'bobr-object-record-v1'"
    ));
}

#[test]
fn parse_object_record_rejects_mismatched_path_key() {
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    let mismatched_object_hash = "2222222222222222222222222222222222222222222222222222222222222222"
        .parse::<ObjectHash>()
        .unwrap();
    let value = json!({
        "schema": OBJECT_RECORD_SCHEMA,
        "build_key": BuildKey::from_object_hash(object_hash),
        "object_hash": object_hash.to_string(),
        "inputs": [],
    });

    assert!(matches!(
        parse_object_record_value(mismatched_object_hash, &value),
        Err(StoreError::InvalidData(message))
            if message.contains("does not match record object hash")
    ));
}

#[test]
fn import_build_reuses_existing_object_record_via_new_build_handle_ref() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let reuse_key = reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]);
    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "hello",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello-1" }),
            &[],
        ),
        reuse_key,
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "hello-copy",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello-2" }),
            &[],
        ),
        reuse_key,
        &second_stage,
        vec![],
    );

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    assert!(layout.object_path_unchecked(first.object_hash).exists());
    assert_eq!(
        fs::read_link(layout.build_ref_path(first.build_key)).unwrap(),
        PathBuf::from("..")
            .join(OBJECT_RECORDS_DIR)
            .join(format!("{}.json", first.object_hash.to_hex()))
    );
    assert!(layout.build_ref_path(second.build_key).exists());
    assert_eq!(
        fs::read_link(layout.build_ref_path(second.build_key)).unwrap(),
        PathBuf::from("..")
            .join(OBJECT_RECORDS_DIR)
            .join(format!("{}.json", second.object_hash.to_hex()))
    );
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("hello-copy")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(second.object_hash.to_hex())
    );
}

#[test]
fn import_build_writes_build_record_and_object_ref() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage = temp.path().join("script.sh");
    fs::write(&stage, b"echo hi\n").unwrap();
    let published = materialize_named_test_build(
        &layout,
        "script",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
            &[parse_build_key(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
            &[],
        ),
        &stage,
        vec![],
    );

    let build_ref_path = layout.build_ref_path(published.build_key);
    let object_record_path = layout.object_record_path(published.object_hash);
    assert!(build_ref_path.exists());
    assert!(object_record_path.exists());
    assert_eq!(
        fs::read_link(&build_ref_path).unwrap(),
        PathBuf::from("..")
            .join(OBJECT_RECORDS_DIR)
            .join(format!("{}.json", published.object_hash.to_hex()))
    );

    let build_json: Value =
        serde_json::from_slice(&fs::read(&object_record_path).unwrap()).unwrap();
    assert_eq!(
        build_json["schema"],
        Value::String(OBJECT_RECORD_SCHEMA.to_string())
    );
    assert_eq!(build_json["run_id"], Value::String(TEST_RUN_ID.to_string()));
    assert_eq!(
        build_json["object_hash"],
        Value::String(published.object_hash.to_string())
    );
    assert_eq!(build_json["inputs"], Value::Array(vec![]));

    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("script")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(published.object_hash.to_hex())
    );
}

#[test]
fn object_record_ref_loaders_reject_non_canonical_targets() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage = temp.path().join("script.sh");
    fs::write(&stage, b"echo hi\n").unwrap();
    let build_key = build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]);
    let reuse_key = reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]);
    let published =
        materialize_named_test_build(&layout, "script", build_key, reuse_key, &stage, vec![]);
    let non_canonical_target = PathBuf::from("..")
        .join("not-object-records")
        .join(format!("{}.json", published.object_hash.to_hex()));

    replace_symlink(&non_canonical_target, &layout.build_ref_path(build_key)).unwrap();
    let error = load_build_handle(&layout, build_key).unwrap_err();
    assert!(error.to_string().contains("build ref"));
    assert!(
        error
            .to_string()
            .contains("non-canonical object record target")
    );

    replace_symlink(&non_canonical_target, &layout.reuse_ref_path(reuse_key)).unwrap();
    let error = load_reuse_object_record(&layout, reuse_key).unwrap_err();
    assert!(error.to_string().contains("reuse ref"));
    assert!(
        error
            .to_string()
            .contains("non-canonical object record target")
    );
}

#[test]
fn object_record_round_trips_inputs() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let inputs = vec![
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111"),
        parse_object_hash("2222222222222222222222222222222222222222222222222222222222222222"),
    ];
    let reuse_key = reuse_key_for(
        "CasTest",
        json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
        &inputs,
    );

    let stage = temp.path().join("script.sh");
    fs::write(&stage, b"echo hi\n").unwrap();
    let published = materialize_named_test_build(
        &layout,
        "script",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
            &[],
        ),
        reuse_key,
        &stage,
        inputs.clone(),
    );

    let loaded = load_object_record(&layout, published.object_hash)
        .unwrap()
        .expect("expected object record to exist");

    assert_eq!(loaded.inputs, inputs);

    let raw: Value = serde_json::from_slice(
        &fs::read(layout.object_record_path(published.object_hash)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        raw["inputs"],
        Value::Array(
            inputs
                .iter()
                .map(|object_hash| Value::String(object_hash.to_string()))
                .collect()
        )
    );
}

#[test]
fn record_existing_source_object_requires_existing_object() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");

    let error =
        record::record_existing_source_object(&layout, object_hash, TEST_RUN_ID).unwrap_err();

    assert!(matches!(error, StoreError::Io(message) if message.contains("source object")));
    assert!(!layout.object_record_path(object_hash).exists());
}

#[test]
fn record_existing_source_object_returns_none_when_object_absent() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");

    let recorded =
        record_existing_source_object(&layout, object_hash, "source", TEST_RUN_ID).unwrap();

    assert!(recorded.is_none());
    assert!(!layout.object_record_path(object_hash).exists());
    assert!(
        load_build_handle(&layout, BuildKey::from_object_hash(object_hash))
            .unwrap()
            .is_none()
    );
}

#[test]
fn record_existing_source_object_reuses_canonical_record() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let object_hash = import_object(&layout, &stage).unwrap();
    record::record_existing_source_object(&layout, object_hash, TEST_RUN_ID).unwrap();

    let hit = record_existing_source_object(&layout, object_hash, "source", TEST_RUN_ID)
        .unwrap()
        .expect("expected source hit");
    assert_eq!(hit, object_hash);
    let record = load_object_record(&layout, object_hash).unwrap().unwrap();
    assert_eq!(record.run_id.as_deref(), Some(TEST_RUN_ID));
    let resolved = load_build_handle(&layout, BuildKey::from_object_hash(object_hash))
        .unwrap()
        .expect("expected source build handle");
    assert_eq!(resolved, object_hash);
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("source")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(object_hash.to_hex())
    );
}

#[test]
fn record_existing_source_object_records_existing_object_as_source_object() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let object_hash = import_object(&layout, &stage).unwrap();
    let object_record_path = layout.object_record_path(object_hash);
    assert!(!object_record_path.exists());

    let hit = record_existing_source_object(&layout, object_hash, "source", TEST_RUN_ID)
        .unwrap()
        .expect("expected source hit");
    assert_eq!(hit, object_hash);
    let record = load_object_record(&layout, object_hash).unwrap().unwrap();
    assert_eq!(record.inputs, Vec::new());
    assert_eq!(record.run_id.as_deref(), Some(TEST_RUN_ID));
    assert!(object_record_path.exists());
    let resolved = load_build_handle(&layout, BuildKey::from_object_hash(object_hash))
        .unwrap()
        .expect("expected source build handle");
    assert_eq!(resolved, object_hash);
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("source")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(object_hash.to_hex())
    );
}

#[test]
fn import_source_object_on_match_imports_object_and_writes_canonical_record() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let object_hash = hash_path(&stage).unwrap();

    let outcome =
        import_source_object(&layout, object_hash, &stage, "source", TEST_RUN_ID).unwrap();

    let SourceImportOutcome::Matched(matched_hash) = outcome else {
        panic!("expected source import match");
    };
    assert_eq!(matched_hash, object_hash);
    let record = load_object_record(&layout, object_hash).unwrap().unwrap();
    assert_eq!(record.run_id.as_deref(), Some(TEST_RUN_ID));
    assert!(layout.object_path_unchecked(object_hash).exists());
    assert!(layout.object_record_path(object_hash).exists());
    let resolved = load_build_handle(&layout, BuildKey::from_object_hash(object_hash))
        .unwrap()
        .expect("expected source build handle");
    assert_eq!(resolved, object_hash);
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("source")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(object_hash.to_hex())
    );
    assert!(!stage.exists());
}

#[test]
fn import_source_object_on_mismatch_imports_actual_object_without_declared_record() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let actual_hash = hash_path(&stage).unwrap();
    let declared_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    assert_ne!(actual_hash, declared_hash);

    let outcome =
        import_source_object(&layout, declared_hash, &stage, "source", TEST_RUN_ID).unwrap();

    let SourceImportOutcome::Mismatched {
        actual_hash: imported_hash,
    } = outcome
    else {
        panic!("expected source import mismatch");
    };
    assert_eq!(imported_hash, actual_hash);
    assert!(layout.object_path_unchecked(actual_hash).exists());
    assert!(!layout.object_record_path(declared_hash).exists());
    assert!(!layout.object_record_path(actual_hash).exists());
    assert!(
        load_build_handle(&layout, BuildKey::from_object_hash(declared_hash))
            .unwrap()
            .is_none()
    );
    assert!(!layout.object_refs_dir().join("source").exists());
    assert!(!stage.exists());
}

#[test]
fn object_ref_update_rejects_non_canonical_current_target() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    materialize_text_object(&layout, temp.path(), "script", "hello");
    let object_ref_path = layout.object_refs_dir().join("script");

    let non_canonical_object = PathBuf::from("..")
        .join("not-objects")
        .join("1111111111111111111111111111111111111111111111111111111111111111");
    replace_symlink(&non_canonical_object, &object_ref_path).unwrap();

    let next_stage = temp.path().join("script-next.txt");
    fs::write(&next_stage, b"next").unwrap();
    let error = import_build(
        &layout,
        build_key_for("CasTest", json!({ "kind": "next" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "next" }), &[]),
        vec![],
        &next_stage,
        "script",
        TEST_RUN_ID,
    )
    .unwrap_err();

    assert!(error.to_string().contains("current object ref"));
    assert!(error.to_string().contains("non-canonical object target"));
}

#[test]
fn same_object_different_payload_produces_different_build_key() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "first",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello" }),
            &[],
        ),
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "second",
        build_key_for(
            "CasTest",
            json!({ "kind": "source-tree", "source": "hello" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "source-tree", "source": "hello" }),
            &[],
        ),
        &second_stage,
        vec![],
    );

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

#[test]
fn build_key_changes_when_kind_changes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "kind-a",
        build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "kind-b",
        build_key_for("CasTest", json!({ "kind": "source-tree" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "source-tree" }), &[]),
        &second_stage,
        vec![],
    );

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

#[test]
fn build_key_changes_when_builder_tag_changes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "producer-a",
        build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "producer-b",
        build_key_for("Sandbox", json!({ "kind": "sandbox-script" }), &[]),
        reuse_key_for("Sandbox", json!({ "kind": "sandbox-script" }), &[]),
        &second_stage,
        vec![],
    );

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

#[test]
fn import_build_rotates_existing_object_refs_into_generations() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "shared",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello" }),
            &[],
        ),
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello world").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "shared",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello world" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello world" }),
            &[],
        ),
        &second_stage,
        vec![],
    );

    assert_ne!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("shared")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(second.object_hash.to_hex())
    );
    let generations = object_ref_generations(&layout, "shared");
    assert_eq!(generations.len(), 1);
    assert_eq!(
        fs::read_link(&generations[0]).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(first.object_hash.to_hex())
    );
}

#[test]
fn import_build_same_object_ref_target_does_not_create_generation_refs() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let build_key = build_key_for(
        "CasTest",
        json!({ "kind": "sandbox-script", "source": "hello" }),
        &[],
    );
    let reuse_key = reuse_key_for(
        "CasTest",
        json!({ "kind": "sandbox-script", "source": "hello" }),
        &[],
    );
    let first = materialize_named_test_build(
        &layout,
        "shared",
        build_key,
        reuse_key,
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "shared",
        build_key,
        reuse_key,
        &second_stage,
        vec![],
    );

    assert_eq!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    assert!(object_ref_generations(&layout, "shared").is_empty());
}

#[test]
fn import_build_generation_suffix_collisions_get_numeric_suffixes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"one").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "shared",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "one" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "one" }),
            &[],
        ),
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"two").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "shared",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "two" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "two" }),
            &[],
        ),
        &second_stage,
        vec![],
    );

    let third_stage = temp.path().join("third.txt");
    fs::write(&third_stage, b"three").unwrap();
    let third = materialize_named_test_build(
        &layout,
        "shared",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "three" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "three" }),
            &[],
        ),
        &third_stage,
        vec![],
    );

    let generations = object_ref_generations(&layout, "shared");
    assert_eq!(generations.len(), 2);
    let targets = generations
        .iter()
        .map(|path| fs::read_link(path).unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        targets,
        BTreeSet::from([
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(first.object_hash.to_hex()),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(second.object_hash.to_hex()),
        ])
    );
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("shared")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(third.object_hash.to_hex())
    );
}

#[test]
fn replace_symlink_replaces_existing_ref_atomically() {
    let temp = tempdir().unwrap();
    let link = temp.path().join("current");
    let old_target = Path::new("../objects/old");
    let new_target = Path::new("../objects/new");

    replace_symlink(old_target, &link).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), old_target);

    replace_symlink(new_target, &link).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), new_target);
}

#[test]
fn replace_symlink_temp_names_do_not_conflict_on_repeated_replace() {
    let temp = tempdir().unwrap();
    let link = temp.path().join("current");

    for index in 0..16 {
        let target = PathBuf::from(format!("../objects/{index}"));
        replace_symlink(&target, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }

    let temp_refs = fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count();
    assert_eq!(temp_refs, 0);
}

#[test]
fn invalid_ref_name_is_rejected() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    for invalid_name in ["", ".", "..", "bad/name", "bad name"] {
        let stage = temp.path().join(format!(
            "invalid-{}.txt",
            invalid_name.replace(['/', ' '], "_")
        ));
        fs::write(&stage, b"hello").unwrap();

        let error = import_build(
            &layout,
            build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            vec![],
            &stage,
            invalid_name,
            TEST_RUN_ID,
        )
        .unwrap_err();

        assert!(matches!(error, StoreError::InvalidInput(_)));
    }
}

#[test]
fn import_build_accepts_directory_objects() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage_dir = temp.path().join("tree");
    fs::create_dir_all(stage_dir.join("bin")).unwrap();
    fs::write(stage_dir.join("bin").join("tool"), b"echo hi\n").unwrap();

    let published = materialize_named_test_build(
        &layout,
        "tree",
        build_key_for("Tree", json!({ "kind": "source-tree" }), &[]),
        reuse_key_for("Tree", json!({ "kind": "source-tree" }), &[]),
        &stage_dir,
        vec![],
    );

    let object_path = layout.object_path_unchecked(published.object_hash);
    assert!(object_path.is_dir());
    assert!(object_path.join("bin").join("tool").exists());
    assert!(layout.build_ref_path(published.build_key).exists());
}

#[test]
fn import_build_points_fs_tree_object_ref_at_object_root() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage_dir = temp.path().join("fs-tree");
    fs::create_dir(&stage_dir).unwrap();
    fs::write(stage_dir.join("manifest.jsonl"), b"{\"schema\":\"test\"}\n").unwrap();
    fs::create_dir(stage_dir.join("root")).unwrap();

    let published = materialize_named_test_build(
        &layout,
        "tree",
        build_key_for("Tree", json!({ "kind": "fs-tree" }), &[]),
        reuse_key_for("Tree", json!({ "kind": "fs-tree" }), &[]),
        &stage_dir,
        vec![],
    );

    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("tree")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(published.object_hash.to_hex())
    );
    let object_path = layout.object_path_unchecked(published.object_hash);
    assert!(object_path.join("manifest.jsonl").is_file());
    assert!(object_path.join("root").is_dir());
}

#[test]
fn store_layout_does_not_create_object_indexes_dir() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    assert!(!layout.root().join("object-indexes").exists());
}

#[test]
fn import_object_does_not_write_leaf_index_when_hashing_staged_path() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage_dir = temp.path().join("stage");
    fs::create_dir(&stage_dir).unwrap();
    fs::write(stage_dir.join("payload"), b"hello\n").unwrap();

    import_object(&layout, &stage_dir).unwrap();

    assert!(!layout.root().join("object-indexes").exists());
}

#[test]
fn import_object_atomically_publishes_a_read_only_directory() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage_dir = temp.path().join("stage");
    fs::create_dir(&stage_dir).unwrap();
    fs::write(stage_dir.join("payload"), b"hello\n").unwrap();
    fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let object_hash = import_object(&layout, &stage_dir).unwrap();

    assert!(!stage_dir.exists());
    let object_path = layout.object_path_unchecked(object_hash);
    assert!(object_path.join("payload").is_file());
    assert_eq!(
        fs::metadata(&object_path).unwrap().permissions().mode() & 0o7777,
        0o555
    );
    assert!(fs::read_dir(layout.objects_dir()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".bobr-import-")
    }));
}

#[test]
fn existing_object_reuse_removes_staged_path() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    materialize_named_test_build(
        &layout,
        "first",
        build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second_stage_path = second_stage.clone();
    materialize_named_test_build(
        &layout,
        "second",
        build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
        &second_stage,
        vec![],
    );

    assert!(!second_stage_path.exists());
}

#[test]
fn build_key_changes_when_input_order_changes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let key_a = parse_build_key("1111111111111111111111111111111111111111111111111111111111111111");
    let key_b = parse_build_key("2222222222222222222222222222222222222222222222222222222222222222");

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "ordered-ab",
        build_key_for(
            "Sandbox",
            json!({ "kind": "sandbox-output" }),
            &[key_a, key_b],
        ),
        reuse_key_for("Sandbox", json!({ "kind": "sandbox-output" }), &[]),
        &first_stage,
        vec![],
    );

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = materialize_named_test_build(
        &layout,
        "ordered-ba",
        build_key_for(
            "Sandbox",
            json!({ "kind": "sandbox-output" }),
            &[key_b, key_a],
        ),
        reuse_key_for("Sandbox", json!({ "kind": "sandbox-output" }), &[]),
        &second_stage,
        vec![],
    );

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

#[test]
fn store_create_creates_full_layout() {
    let temp = tempdir().unwrap();

    let layout = Store::create(temp.path()).unwrap();

    assert_eq!(layout.root(), temp.path());
    assert!(layout.objects_dir().is_dir());
    assert!(layout.builds_dir().is_dir());
    assert!(layout.object_refs_dir().is_dir());
    assert!(fs_files_dir(&layout).is_dir());
    assert!(fs_trees_dir(&layout).is_dir());
    assert!(fs_tree_refs_dir(&layout).is_dir());
    // Where a build writes its logs and scratch belongs to a run, not here.
    assert!(!temp.path().join("logs").exists());
    assert!(!temp.path().join("tmp").exists());
}

#[test]
fn store_fs_tree_round_trips_through_serde() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let fs_tree = layout.fs_tree();

    let value = serde_json::to_value(&fs_tree).unwrap();
    assert_eq!(value["root"], temp.path().display().to_string());

    let decoded: FsTree = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, fs_tree);
}

#[test]
fn store_fs_tree_deserialize_rejects_bad_roots() {
    let temp = tempdir().unwrap();

    assert!(serde_json::from_value::<FsTree>(json!({"root": "relative"})).is_err());
    assert!(
        serde_json::from_value::<FsTree>(
            json!({"root": temp.path().join("missing").display().to_string()})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<FsTree>(json!({"root": temp.path().display().to_string()}))
            .is_err()
    );
}

#[test]
fn store_fs_tree_imports_with_install_into_store_fs_files() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("payload"), b"payload\n").unwrap();
    let owner = fs::symlink_metadata(temp.path()).unwrap();
    let install = FsTreeInstall {
        rules: vec![FsTreeInstallRule {
            path: "**".to_string(),
            attrs: FsTreeInstallAttrs {
                uid: Some(owner.uid()),
                gid: Some(owner.gid()),
                directory_mode: Some(0o755),
                regular_file_mode: Some(0o644),
                executable_file_mode: Some(0o755),
            },
        }],
    };

    let manifest = layout
        .fs_tree()
        .import_with_install(&source, &install)
        .unwrap();

    assert!(manifest.entries().contains(&FsTreeEntry::directory(
        "",
        owner.uid(),
        owner.gid(),
        0o755,
    )));
    let hash = manifest
        .entries()
        .iter()
        .find_map(|entry| match entry {
            FsTreeEntry::File { path, hash } if path == "payload" => Some(*hash),
            _ => None,
        })
        .expect("payload file entry");
    let hex = hash.to_hex();
    assert!(fs_files_dir(&layout).join(&hex[..2]).join(hex).is_file());
}

#[test]
fn store_handle_is_send_sync_and_clone() {
    fn assert_send_sync_clone<T: Send + Sync + Clone>() {}

    assert_send_sync_clone::<Store>();
}

#[test]
fn store_create_requires_existing_absolute_root() {
    let temp = tempdir().unwrap();
    let missing = temp.path().join("missing-store");

    assert!(matches!(
        Store::create(Path::new("relative-store")),
        Err(StoreError::InvalidInput(message))
            if message.contains("store root must be absolute")
    ));
    assert!(matches!(
        Store::create(&missing),
        Err(StoreError::InvalidInput(message))
            if message.contains("store root must exist")
    ));
}

#[test]
fn store_create_rejects_dangling_symlink_root() {
    let temp = tempdir().unwrap();
    let dangling = temp.path().join("current-store");
    symlink(temp.path().join("missing-store"), &dangling).unwrap();

    assert!(matches!(
        Store::create(&dangling),
        Err(StoreError::InvalidInput(message))
            if message.contains("store root must exist")
    ));
}

#[test]
fn store_create_rejects_symlink_to_file_root() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("not-a-store");
    fs::write(&file, b"not a directory").unwrap();
    let link = temp.path().join("current-store");
    symlink(&file, &link).unwrap();

    assert!(matches!(
        Store::create(&link),
        Err(StoreError::InvalidInput(message))
            if message.contains("store root must be a directory")
    ));
}

#[test]
fn store_create_pins_symlink_root_to_initial_target() {
    let temp = tempdir().unwrap();
    let store_a = temp.path().join("store-a");
    let store_b = temp.path().join("store-b");
    fs::create_dir(&store_a).unwrap();
    fs::create_dir(&store_b).unwrap();

    let link = temp.path().join("current-store");
    symlink(&store_a, &link).unwrap();
    let store = Store::create(&link).unwrap();

    fs::remove_file(&link).unwrap();
    symlink(&store_b, &link).unwrap();

    let stage = temp.path().join("payload");
    fs::write(&stage, b"payload\n").unwrap();
    let object_hash = import_object(&store, &stage).unwrap();

    // The handle keeps writing to the target it resolved at creation.
    assert!(
        store_a
            .join(OBJECTS_DIR)
            .join(object_hash.to_hex())
            .is_file()
    );
    assert!(!store_b.join(OBJECTS_DIR).exists());
}

#[test]
fn store_create_rejects_non_directory_layout_entry() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join(OBJECTS_DIR), b"not a directory").unwrap();

    assert!(matches!(
        Store::create(temp.path()),
        Err(StoreError::InvalidData(message))
            if message.contains("store objects path")
    ));
}

#[test]
fn build_key_display_and_parse_roundtrip() {
    let key =
        BuildKey::from_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();

    assert_eq!(
        key.to_string(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        BuildKey::from_str(&key.to_string()).unwrap().as_bytes(),
        key.as_bytes()
    );
}

#[test]
fn executable_bit_changes_object_hash_for_distinct_invocations() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("plain.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = materialize_named_test_build(
        &layout,
        "plain",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello", "variant": "plain" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello", "variant": "plain" }),
            &[],
        ),
        &first_stage,
        vec![],
    );

    let exec_stage = temp.path().join("exec.txt");
    fs::write(&exec_stage, b"hello").unwrap();
    fs::set_permissions(&exec_stage, fs::Permissions::from_mode(0o755)).unwrap();
    let second = materialize_named_test_build(
        &layout,
        "exec",
        build_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello", "variant": "exec" }),
            &[],
        ),
        reuse_key_for(
            "CasTest",
            json!({ "kind": "sandbox-script", "source": "hello", "variant": "exec" }),
            &[],
        ),
        &exec_stage,
        vec![],
    );

    assert_ne!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

fn materialize_text_object(
    layout: &Store,
    temp_root: &Path,
    object_ref_name: &str,
    text: &str,
) -> TestBuild {
    let stage = temp_root.join(format!("{object_ref_name}.txt"));
    fs::write(&stage, text.as_bytes()).unwrap();
    let payload = json!({
        "kind": "text-output",
        "name": object_ref_name,
        "text": text,
    });
    let build_key = build_key_for("CasTest", payload.clone(), &[]);
    let object_hash = import_build(
        layout,
        build_key,
        reuse_key_for("CasTest", payload, &[]),
        vec![],
        &stage,
        object_ref_name,
        TEST_RUN_ID,
    )
    .unwrap();
    TestBuild {
        object_hash,
        build_key,
    }
}

fn materialize_named_test_build(
    layout: &Store,
    object_ref_name: &str,
    build_key: BuildKey,
    reuse_key: ReuseKey,
    staged_path: &Path,
    inputs: Vec<ObjectHash>,
) -> TestBuild {
    let object_hash = import_build(
        layout,
        build_key,
        reuse_key,
        inputs,
        staged_path,
        object_ref_name,
        TEST_RUN_ID,
    )
    .unwrap();
    TestBuild {
        object_hash,
        build_key,
    }
}

fn object_ref_generations(layout: &Store, name: &str) -> Vec<PathBuf> {
    let mut generations = fs::read_dir(layout.object_refs_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("{name}."))
        })
        .collect::<Vec<_>>();
    generations.sort();
    generations
}

fn parse_object_hash(value: &str) -> ObjectHash {
    ObjectHash::from_str(value).unwrap()
}

fn parse_build_key(value: &str) -> BuildKey {
    BuildKey::from_str(value).unwrap()
}

fn build_key_for(builder_tag: &str, payload: Value, input_builds: &[BuildKey]) -> BuildKey {
    let inputs = input_builds
        .iter()
        .enumerate()
        .map(|(i, key)| (format!("in{i:03}"), *key))
        .collect::<BTreeMap<_, _>>();
    compute_build_key(
        builder_tag,
        "test",
        ConfigDigest::of(&payload).unwrap(),
        &inputs,
    )
    .unwrap()
}

fn reuse_key_for(builder_tag: &str, payload: Value, inputs: &[ObjectHash]) -> ReuseKey {
    let inputs = inputs
        .iter()
        .enumerate()
        .map(|(i, hash)| (format!("in{i:03}"), *hash))
        .collect::<BTreeMap<_, _>>();
    compute_reuse_key(
        builder_tag,
        "test",
        ConfigDigest::of(&payload).unwrap(),
        &inputs,
    )
    .unwrap()
}
