use super::*;
use crate::identity::{
    BuildKey, ResultId, ReuseKey, compute_build_key, compute_result_id, compute_reuse_key,
};
use fsobj_hash::ObjectHash;
use fsobj_hash::hash_path;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use tempfile::tempdir;

fn create_test_store(root: &Path) -> Store {
    let store_root = root.join(".bobr");
    fs::create_dir_all(&store_root).unwrap();
    Store::create(&store_root).unwrap()
}

#[test]
fn canonical_json_hash_is_stable_across_key_order() {
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    let left = json!({
        "z": 1,
        "a": true,
        "result": build_json_value(Some(sample_created_at()), object_hash, &[]),
    });
    let right = json!({
        "result": build_json_value(Some(sample_created_at()), object_hash, &[]),
        "a": true,
        "z": 1,
    });

    assert_eq!(
        canonical_json_bytes(&left).unwrap(),
        canonical_json_bytes(&right).unwrap()
    );
}

#[test]
fn reuse_key_is_stable_for_identical_inputs() {
    let payload = json!({ "kind": "sandbox-script" });
    let inputs = vec![
        ReuseInputIdentity {
            object_hash: parse_object_hash(
                "1111111111111111111111111111111111111111111111111111111111111111",
            ),
        },
        ReuseInputIdentity {
            object_hash: parse_object_hash(
                "2222222222222222222222222222222222222222222222222222222222222222",
            ),
        },
    ];

    assert_eq!(
        compute_reuse_key("CasTest", &payload, &inputs).unwrap(),
        compute_reuse_key("CasTest", &payload, &inputs).unwrap()
    );
}

#[test]
fn parse_result_record_rejects_old_schema() {
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    let result_id = compute_result_id(object_hash);
    let value = json!({
        "schema": "bobr-result-v4",
        "result_id": result_id.to_string(),
        "object_hash": object_hash.to_string(),
        "inputs": [],
    });

    assert!(matches!(
        parse_result_record_value(result_id, &value),
        Err(StoreError::InvalidData(message))
            if message == "unsupported result record schema 'bobr-result-v4'"
    ));
}

#[test]
fn parse_result_record_rejects_mismatched_path_key() {
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    let mismatched_result_id = "2222222222222222222222222222222222222222222222222222222222222222"
        .parse::<ResultId>()
        .unwrap();
    let value = json!({
        "schema": RESULT_SCHEMA,
        "object_hash": object_hash.to_string(),
        "inputs": [],
    });

    assert!(matches!(
        parse_result_record_value(mismatched_result_id, &value),
        Err(StoreError::InvalidData(message))
            if message.contains("does not match object hash")
    ));
}

#[test]
fn publish_build_reuses_existing_result_via_new_build_handle_ref() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let reuse_key =
        compute_reuse_key("CasTest", &json!({ "kind": "sandbox-script" }), &[]).unwrap();
    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "hello".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello-1" }),
                &[],
            ),
            reuse_key,
            created_at: sample_created_at().to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "hello-copy".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello-2" }),
                &[],
            ),
            reuse_key,
            created_at: sample_created_at().to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
    assert_eq!(first.result_id, second.result_id);
    assert!(layout.object_path(first.object_hash).exists());
    assert_eq!(
        fs::read_link(layout.build_ref_path(first.build_key)).unwrap(),
        PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", first.result_id.to_hex()))
    );
    assert!(layout.build_ref_path(second.build_key).exists());
    assert_eq!(
        fs::read_link(layout.build_ref_path(second.build_key)).unwrap(),
        PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", second.result_id.to_hex()))
    );
    assert_eq!(
        fs::read_link(layout.result_refs_dir().join("hello-copy.json")).unwrap(),
        PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", second.result_id.to_hex()))
    );
}

#[test]
fn publish_build_writes_build_record_and_refs() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage = temp.path().join("script.sh");
    fs::write(&stage, b"echo hi\n").unwrap();
    let published = publish_build(
        &layout,
        PublishRequest {
            publication_name: "script".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
                &[parse_build_key(
                    "1111111111111111111111111111111111111111111111111111111111111111",
                )],
            ),
            reuse_key: compute_reuse_key(
                "CasTest",
                &json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
                &[],
            )
            .unwrap(),
            created_at: sample_created_at().to_string(),
            staged_path: stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let build_ref_path = layout.build_ref_path(published.build_key);
    let result_path = layout.result_record_path(published.result_id);
    assert!(build_ref_path.exists());
    assert!(result_path.exists());
    assert_eq!(
        fs::read_link(&build_ref_path).unwrap(),
        PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", published.result_id.to_hex()))
    );

    let build_json: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
    assert_eq!(
        build_json["schema"],
        Value::String(BUILD_SCHEMA.to_string())
    );
    assert!(build_json.get("result_id").is_none());
    assert_eq!(
        build_json["created_at"],
        Value::String(sample_created_at().to_string())
    );
    assert_eq!(
        build_json["object_hash"],
        Value::String(published.object_hash.to_string())
    );
    assert_eq!(build_json["inputs"], Value::Array(vec![]));

    assert_eq!(
        fs::read_link(layout.result_refs_dir().join("script.json")).unwrap(),
        PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", published.result_id.to_hex()))
    );
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("script")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(published.object_hash.to_hex())
    );
}

#[test]
fn result_ref_loaders_reject_non_canonical_targets() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage = temp.path().join("script.sh");
    fs::write(&stage, b"echo hi\n").unwrap();
    let build_key = build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]);
    let reuse_key = reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]);
    let published = publish_build(
        &layout,
        PublishRequest {
            publication_name: "script".to_string(),
            build_key,
            reuse_key,
            created_at: sample_created_at().to_string(),
            staged_path: stage,
            inputs: vec![],
        },
    )
    .unwrap();
    let non_canonical_target = PathBuf::from("..")
        .join("not-results")
        .join(format!("{}.json", published.result_id.to_hex()));

    replace_symlink(&non_canonical_target, &layout.build_ref_path(build_key)).unwrap();
    let error = load_build_handle(&layout, build_key).unwrap_err();
    assert!(error.to_string().contains("build ref"));
    assert!(error.to_string().contains("non-canonical result target"));

    replace_symlink(&non_canonical_target, &layout.reuse_ref_path(reuse_key)).unwrap();
    let error = load_reuse_record(&layout, reuse_key).unwrap_err();
    assert!(error.to_string().contains("reuse ref"));
    assert!(error.to_string().contains("non-canonical result target"));

    replace_symlink(
        &non_canonical_target,
        &layout.result_refs_dir().join("script.json"),
    )
    .unwrap();
    let error = load_current_publication(&layout, "script").unwrap_err();
    assert!(error.to_string().contains("current result ref"));
    assert!(error.to_string().contains("non-canonical result target"));
}

#[test]
fn result_record_round_trips_inputs() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let inputs = vec![
        ReuseInputIdentity {
            object_hash: parse_object_hash(
                "1111111111111111111111111111111111111111111111111111111111111111",
            ),
        },
        ReuseInputIdentity {
            object_hash: parse_object_hash(
                "2222222222222222222222222222222222222222222222222222222222222222",
            ),
        },
    ];
    let reuse_key = compute_reuse_key(
        "CasTest",
        &json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
        &inputs,
    )
    .unwrap();

    let stage = temp.path().join("script.sh");
    fs::write(&stage, b"echo hi\n").unwrap();
    let published = publish_build(
        &layout,
        PublishRequest {
            publication_name: "script".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "echo hi\n" }),
                &[],
            ),
            reuse_key,
            created_at: sample_created_at().to_string(),
            staged_path: stage,
            inputs: inputs.clone(),
        },
    )
    .unwrap();

    let loaded = load_result_record(&layout, published.result_id)
        .unwrap()
        .expect("expected result record to exist");

    assert_eq!(loaded.inputs, inputs);
}

#[test]
fn record_existing_source_result_requires_existing_object() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");

    let error =
        crate::record::record_existing_source_result(&layout, object_hash, sample_created_at())
            .unwrap_err();

    assert!(matches!(error, StoreError::Io(message) if message.contains("source object")));
    assert!(
        !layout
            .result_record_path(compute_result_id(object_hash))
            .exists()
    );
}

#[test]
fn lookup_source_result_returns_missing_when_result_and_object_absent() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");

    let lookup = lookup_source_result(&layout, object_hash, sample_created_at()).unwrap();

    assert!(matches!(lookup, SourceLookup::Missing));
    assert!(
        !layout
            .result_record_path(compute_result_id(object_hash))
            .exists()
    );
}

#[test]
fn lookup_source_result_reuses_canonical_result() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let object_hash = import_object(&layout, &stage).unwrap();
    let stored = crate::record::record_existing_source_result(
        &layout,
        object_hash,
        "2026-03-24T13:00:00.000000000Z",
    )
    .unwrap();

    let lookup =
        lookup_source_result(&layout, object_hash, "2026-03-24T14:00:00.000000000Z").unwrap();

    let SourceLookup::Hit(hit) = lookup else {
        panic!("expected source hit");
    };
    assert_eq!(hit.result_id(), stored.result_id());
    assert_eq!(
        hit.result.created_at.as_deref(),
        Some("2026-03-24T13:00:00.000000000Z")
    );
}

#[test]
fn lookup_source_result_records_existing_object_as_source_result() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let object_hash = import_object(&layout, &stage).unwrap();
    let result_path = layout.result_record_path(compute_result_id(object_hash));
    assert!(!result_path.exists());

    let lookup = lookup_source_result(&layout, object_hash, sample_created_at()).unwrap();

    let SourceLookup::Hit(hit) = lookup else {
        panic!("expected source hit");
    };
    assert_eq!(hit.result.object_hash, object_hash);
    assert_eq!(hit.result.inputs, Vec::new());
    assert_eq!(hit.result.created_at.as_deref(), Some(sample_created_at()));
    assert!(result_path.exists());
}

#[test]
fn import_source_result_on_match_imports_object_and_writes_canonical_result() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let object_hash = hash_path(&stage).unwrap();

    let outcome = import_source_result(&layout, object_hash, &stage, sample_created_at()).unwrap();

    let SourceImportOutcome::Matched(stored) = outcome else {
        panic!("expected source import match");
    };
    assert_eq!(stored.result.object_hash, object_hash);
    assert_eq!(
        stored.result.created_at.as_deref(),
        Some(sample_created_at())
    );
    assert!(layout.object_path(object_hash).exists());
    assert!(
        layout
            .result_record_path(compute_result_id(object_hash))
            .exists()
    );
    assert!(!stage.exists());
}

#[test]
fn import_source_result_on_mismatch_imports_actual_object_without_declared_result() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let stage = temp.path().join("source.txt");
    fs::write(&stage, b"hello").unwrap();
    let actual_hash = hash_path(&stage).unwrap();
    let declared_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    assert_ne!(actual_hash, declared_hash);

    let outcome =
        import_source_result(&layout, declared_hash, &stage, sample_created_at()).unwrap();

    let SourceImportOutcome::Mismatched {
        actual_hash: imported_hash,
    } = outcome
    else {
        panic!("expected source import mismatch");
    };
    assert_eq!(imported_hash, actual_hash);
    assert!(layout.object_path(actual_hash).exists());
    assert!(
        !layout
            .result_record_path(compute_result_id(declared_hash))
            .exists()
    );
    assert!(
        !layout
            .result_record_path(compute_result_id(actual_hash))
            .exists()
    );
    assert!(!stage.exists());
}

#[test]
fn publish_result_rejects_result_with_missing_object_without_refs() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let object_hash =
        parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
    let result = ResultRecord {
        object_hash,
        created_at: Some(sample_created_at().to_string()),
        inputs: Vec::new(),
    };
    let result_id = result.result_id();
    crate::record::store_result_record(&layout, &result).unwrap();

    let error = publish_result(&layout, "missing", result_id).unwrap_err();

    assert!(matches!(error, StoreError::Io(message) if message.contains("missing object")));
    assert!(!layout.object_refs_dir().join("missing").exists());
    assert!(!layout.result_refs_dir().join("missing.json").exists());
}

#[test]
fn load_publication_returns_none_when_refs_are_absent() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    assert!(load_publication(&layout, "missing").unwrap().is_none());
}

#[test]
fn load_publication_loads_publication() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let published = publish_text_output(&layout, temp.path(), "script", "hello");

    let loaded = load_publication(&layout, "script")
        .unwrap()
        .expect("expected publication");

    assert_eq!(loaded.result_id(), published.result_id);
    assert_eq!(loaded.result.object_hash, published.object_hash);
    assert!(loaded.object_path.exists());
}

#[test]
fn load_publication_rejects_missing_object_ref() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    publish_text_output(&layout, temp.path(), "script", "hello");
    fs::remove_file(layout.object_refs_dir().join("script")).unwrap();

    let error = load_publication(&layout, "script").unwrap_err();

    assert!(matches!(
        error,
        StoreError::InvalidData(message)
            if message.contains("publication 'script'")
                && message.contains("missing object ref")
    ));
}

#[test]
fn load_publication_rejects_object_ref_hash_mismatch() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let published = publish_text_output(&layout, temp.path(), "script", "hello");
    let other_stage = temp.path().join("other.txt");
    fs::write(&other_stage, b"other").unwrap();
    let other_hash = import_object(&layout, &other_stage).unwrap();
    assert_ne!(published.object_hash, other_hash);

    replace_symlink(
        &PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(other_hash.to_hex()),
        &layout.object_refs_dir().join("script"),
    )
    .unwrap();

    let error = load_publication(&layout, "script").unwrap_err();

    assert!(matches!(
        error,
        StoreError::InvalidData(message)
            if message.contains("object ref points to")
                && message.contains("records")
    ));
}

#[test]
fn load_publication_rejects_non_canonical_result_and_object_refs() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let published = publish_text_output(&layout, temp.path(), "script", "hello");
    let result_ref_path = layout.result_refs_dir().join("script.json");
    let object_ref_path = layout.object_refs_dir().join("script");

    let non_canonical_result = PathBuf::from("..")
        .join("not-results")
        .join(format!("{}.json", published.result_id.to_hex()));
    replace_symlink(&non_canonical_result, &result_ref_path).unwrap();
    let error = load_publication(&layout, "script").unwrap_err();
    assert!(error.to_string().contains("publication result ref"));
    assert!(error.to_string().contains("non-canonical result target"));

    replace_symlink(
        &PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", published.result_id.to_hex())),
        &result_ref_path,
    )
    .unwrap();
    let non_canonical_object = PathBuf::from("..")
        .join("not-objects")
        .join(published.object_hash.to_hex());
    replace_symlink(&non_canonical_object, &object_ref_path).unwrap();
    let error = load_publication(&layout, "script").unwrap_err();
    assert!(error.to_string().contains("publication object ref"));
    assert!(error.to_string().contains("non-canonical object target"));
}

#[test]
fn same_object_different_payload_produces_different_build_key() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "first".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello" }),
                &[],
            ),
            created_at: sample_created_at().to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "second".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "source-tree", "source": "hello" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "source-tree", "source": "hello" }),
                &[],
            ),
            created_at: sample_created_at().to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

#[test]
fn build_key_changes_when_kind_changes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "kind-a".to_string(),
            build_key: build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            reuse_key: reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "kind-b".to_string(),
            build_key: build_key_for("CasTest", json!({ "kind": "source-tree" }), &[]),
            reuse_key: reuse_key_for("CasTest", json!({ "kind": "source-tree" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

#[test]
fn build_key_changes_when_builder_tag_changes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "producer-a".to_string(),
            build_key: build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            reuse_key: reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "producer-b".to_string(),
            build_key: build_key_for("Sandbox", json!({ "kind": "sandbox-script" }), &[]),
            reuse_key: reuse_key_for("Sandbox", json!({ "kind": "sandbox-script" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    assert_eq!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

#[test]
fn publish_build_rotates_existing_refs_into_generations() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "shared".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello" }),
                &[],
            ),
            created_at: "2026-03-24T12:34:56.123456789Z".to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello world").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "shared".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello world" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello world" }),
                &[],
            ),
            created_at: "2026-03-24T12:35:30.123456789Z".to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let suffix = human_timestamp_from_rfc3339("2026-03-24T12:34:56.123456789Z").unwrap();
    assert_ne!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("shared")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(second.object_hash.to_hex())
    );
    assert_eq!(
        fs::read_link(layout.result_refs_dir().join("shared.json")).unwrap(),
        PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", second.result_id.to_hex()))
    );
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join(format!("shared.{suffix}"))).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(first.object_hash.to_hex())
    );
    assert_eq!(
        fs::read_link(
            layout
                .result_refs_dir()
                .join(format!("shared.{suffix}.json"))
        )
        .unwrap(),
        PathBuf::from("..")
            .join(RESULTS_DIR)
            .join(format!("{}.json", first.result_id.to_hex()))
    );
}

#[test]
fn publish_build_same_build_key_does_not_create_generation_refs() {
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
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "shared".to_string(),
            build_key,
            reuse_key,
            created_at: "2026-03-24T12:34:56.123456789Z".to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "shared".to_string(),
            build_key,
            reuse_key,
            created_at: "2026-03-24T12:35:30.123456789Z".to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    assert_eq!(first.build_key, second.build_key);
    assert_eq!(first.object_hash, second.object_hash);
    let suffix = human_timestamp_from_rfc3339("2026-03-24T12:34:56.123456789Z").unwrap();
    assert!(
        !layout
            .object_refs_dir()
            .join(format!("shared.{suffix}"))
            .exists()
    );
    assert!(
        !layout
            .result_refs_dir()
            .join(format!("shared.{suffix}.json"))
            .exists()
    );
}

#[test]
fn publish_build_generation_suffix_collisions_get_numeric_suffixes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"one").unwrap();
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "shared".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "one" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "one" }),
                &[],
            ),
            created_at: "2026-03-24T12:34:56.100000000Z".to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"two").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "shared".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "two" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "two" }),
                &[],
            ),
            created_at: "2026-03-24T12:34:56.200000000Z".to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let third_stage = temp.path().join("third.txt");
    fs::write(&third_stage, b"three").unwrap();
    let third = publish_build(
        &layout,
        PublishRequest {
            publication_name: "shared".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "three" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "three" }),
                &[],
            ),
            created_at: "2026-03-24T12:34:56.300000000Z".to_string(),
            staged_path: third_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let suffix = human_timestamp_from_rfc3339("2026-03-24T12:34:56.100000000Z").unwrap();
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join(format!("shared.{suffix}"))).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(first.object_hash.to_hex())
    );
    assert_eq!(
        fs::read_link(layout.object_refs_dir().join(format!("shared.{suffix}.2"))).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(second.object_hash.to_hex())
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
fn invalid_publication_name_is_rejected() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    for invalid_name in ["", ".", "..", "bad/name", "bad name"] {
        let stage = temp.path().join(format!(
            "invalid-{}.txt",
            invalid_name.replace(['/', ' '], "_")
        ));
        fs::write(&stage, b"hello").unwrap();

        let error = publish_build(
            &layout,
            PublishRequest {
                publication_name: invalid_name.to_string(),
                build_key: build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
                reuse_key: reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: stage,
                inputs: vec![],
            },
        )
        .unwrap_err();

        assert!(matches!(error, StoreError::InvalidInput(_)));
    }
}

#[test]
fn publish_build_accepts_directory_objects() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage_dir = temp.path().join("tree");
    fs::create_dir_all(stage_dir.join("bin")).unwrap();
    fs::write(stage_dir.join("bin").join("tool"), b"echo hi\n").unwrap();

    let published = publish_build(
        &layout,
        PublishRequest {
            publication_name: "tree".to_string(),
            build_key: build_key_for("Tree", json!({ "kind": "source-tree" }), &[]),
            reuse_key: reuse_key_for("Tree", json!({ "kind": "source-tree" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: stage_dir,
            inputs: vec![],
        },
    )
    .unwrap();

    let object_path = layout.object_path(published.object_hash);
    assert!(object_path.is_dir());
    assert!(object_path.join("bin").join("tool").exists());
    assert!(layout.build_ref_path(published.build_key).exists());
}

#[test]
fn materialize_build_trusted_hash_accepts_unreadable_object() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage_dir = temp.path().join("tree");
    fs::create_dir_all(stage_dir.join("private")).unwrap();
    fs::write(stage_dir.join("private").join("secret"), b"secret\n").unwrap();
    let object_hash = hash_path(&stage_dir).unwrap();
    fs::set_permissions(stage_dir.join("private"), fs::Permissions::from_mode(0o000)).unwrap();

    let build_key = build_key_for("Tree", json!({ "kind": "private-tree" }), &[]);
    let reuse_key = reuse_key_for("Tree", json!({ "kind": "private-tree" }), &[]);
    let published = materialize_build_with_trusted_hash(
        &layout,
        build_key,
        reuse_key,
        sample_created_at(),
        vec![],
        &stage_dir,
        object_hash,
    )
    .unwrap();

    assert_eq!(published.build.object_hash, object_hash);
    let object_path = layout.object_path(object_hash);
    assert!(object_path.join("private").exists());

    fs::set_permissions(
        object_path.join("private"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[test]
fn publish_build_points_fs_tree_object_ref_at_object_root() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let stage_dir = temp.path().join("fs-tree");
    fs::create_dir(&stage_dir).unwrap();
    fs::write(stage_dir.join("manifest.jsonl"), b"{\"schema\":\"test\"}\n").unwrap();
    fs::create_dir(stage_dir.join("root")).unwrap();

    let published = publish_build(
        &layout,
        PublishRequest {
            publication_name: "tree".to_string(),
            build_key: build_key_for("Tree", json!({ "kind": "fs-tree" }), &[]),
            reuse_key: reuse_key_for("Tree", json!({ "kind": "fs-tree" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: stage_dir,
            inputs: vec![],
        },
    )
    .unwrap();

    assert_eq!(
        fs::read_link(layout.object_refs_dir().join("tree")).unwrap(),
        PathBuf::from("..")
            .join(OBJECTS_DIR)
            .join(published.object_hash.to_hex())
    );
    let object_path = layout.object_path(published.object_hash);
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
fn existing_object_reuse_removes_staged_path() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    publish_build(
        &layout,
        PublishRequest {
            publication_name: "first".to_string(),
            build_key: build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            reuse_key: reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second_stage_path = second_stage.clone();
    publish_build(
        &layout,
        PublishRequest {
            publication_name: "second".to_string(),
            build_key: build_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            reuse_key: reuse_key_for("CasTest", json!({ "kind": "sandbox-script" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    assert!(!second_stage_path.exists());
}

#[test]
fn existing_trusted_object_reuse_leaves_staged_path_for_runtime_cleanup() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let object_hash = hash_path(&first_stage).unwrap();
    materialize_build_with_trusted_hash(
        &layout,
        build_key_for("CasTest", json!({ "kind": "first" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "first" }), &[]),
        sample_created_at(),
        vec![],
        &first_stage,
        object_hash,
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second_stage_path = second_stage.clone();
    materialize_build_with_trusted_hash(
        &layout,
        build_key_for("CasTest", json!({ "kind": "second" }), &[]),
        reuse_key_for("CasTest", json!({ "kind": "second" }), &[]),
        sample_created_at(),
        vec![],
        &second_stage,
        object_hash,
    )
    .unwrap();

    assert!(second_stage_path.exists());
}

#[test]
fn build_key_changes_when_input_build_key_order_changes() {
    let temp = tempdir().unwrap();
    let layout = create_test_store(temp.path());
    let key_a = parse_build_key("1111111111111111111111111111111111111111111111111111111111111111");
    let key_b = parse_build_key("2222222222222222222222222222222222222222222222222222222222222222");

    let first_stage = temp.path().join("first.txt");
    fs::write(&first_stage, b"hello").unwrap();
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "ordered-ab".to_string(),
            build_key: build_key_for(
                "Sandbox",
                json!({ "kind": "sandbox-output" }),
                &[key_a, key_b],
            ),
            reuse_key: reuse_key_for("Sandbox", json!({ "kind": "sandbox-output" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let second_stage = temp.path().join("second.txt");
    fs::write(&second_stage, b"hello").unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "ordered-ba".to_string(),
            build_key: build_key_for(
                "Sandbox",
                json!({ "kind": "sandbox-output" }),
                &[key_b, key_a],
            ),
            reuse_key: reuse_key_for("Sandbox", json!({ "kind": "sandbox-output" }), &[]),
            created_at: sample_created_at().to_string(),
            staged_path: second_stage,
            inputs: vec![],
        },
    )
    .unwrap();

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
    assert!(layout.result_refs_dir().is_dir());
    assert!(layout.object_refs_dir().is_dir());
    assert!(layout.fs_files_dir().is_dir());
    assert!(layout.fs_trees_dir().is_dir());
    assert!(temp.path().join(LOGS_DIR).is_dir());
    assert!(temp.path().join(TMP_DIR).is_dir());
    assert!(layout.run_log_dir().is_dir());
    assert!(layout.run_tmp_dir().is_dir());
    assert!(layout.run_log_dir().starts_with(temp.path().join(LOGS_DIR)));
    assert!(layout.run_tmp_dir().starts_with(temp.path().join(TMP_DIR)));
}

#[test]
fn store_create_allocates_unique_run_directories() {
    let temp = tempdir().unwrap();

    let first = Store::create(temp.path()).unwrap();
    let second = Store::create(temp.path()).unwrap();

    assert_ne!(first.run_log_dir(), second.run_log_dir());
    assert_ne!(first.run_tmp_dir(), second.run_tmp_dir());
    assert!(first.run_log_dir().is_dir());
    assert!(second.run_log_dir().is_dir());
    assert!(first.run_tmp_dir().is_dir());
    assert!(second.run_tmp_dir().is_dir());
    assert_eq!(
        first.run_log_dir().file_name().unwrap(),
        first.run_tmp_dir().file_name().unwrap()
    );
    assert_eq!(
        second.run_log_dir().file_name().unwrap(),
        second.run_tmp_dir().file_name().unwrap()
    );
}

#[test]
fn run_dir_allocation_uses_numeric_disambiguation_across_logs_and_tmp() {
    let temp = tempdir().unwrap();
    let logs_dir = temp.path().join(LOGS_DIR);
    let tmp_dir = temp.path().join(TMP_DIR);
    fs::create_dir_all(&logs_dir).unwrap();
    fs::create_dir_all(&tmp_dir).unwrap();
    fs::create_dir(logs_dir.join("260603123456")).unwrap();
    fs::create_dir(tmp_dir.join("260603123456.1")).unwrap();

    let (allocated_logs, allocated_tmp) =
        crate::store::create_run_dirs(&logs_dir, &tmp_dir, "260603123456").unwrap();

    assert_eq!(allocated_logs.file_name().unwrap(), "260603123456.2");
    assert_eq!(allocated_tmp.file_name().unwrap(), "260603123456.2");
    assert!(allocated_logs.is_dir());
    assert!(allocated_tmp.is_dir());
    assert!(!logs_dir.join("260603123456.1").exists());
    assert!(tmp_dir.join("260603123456.1").is_dir());
}

#[test]
fn store_exposes_run_log_locations() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let locations = layout.run_log_locations();

    assert_eq!(locations.run_log_dir(), layout.run_log_dir());
    assert_eq!(locations.created_at(), layout.created_at());
    assert!(locations.run_log_dir().is_dir());
}

#[test]
fn store_clone_shares_run_directories_and_serial_counter() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let clone = layout.clone();

    let first = create_workspace(&layout, "Tree", Some("left".to_string()), "build-left").unwrap();
    let second =
        create_workspace(&clone, "Tree", Some("right".to_string()), "build-right").unwrap();

    assert!(first.log_dir().starts_with(layout.run_log_dir()));
    assert!(second.log_dir().starts_with(layout.run_log_dir()));
    assert!(first.temp_dir().starts_with(layout.run_tmp_dir()));
    assert!(second.temp_dir().starts_with(layout.run_tmp_dir()));
    assert_eq!(
        first.log_dir().file_name().unwrap().to_str().unwrap(),
        "00000000-Tree-left"
    );
    assert_eq!(
        second.log_dir().file_name().unwrap().to_str().unwrap(),
        "00000001-Tree-right"
    );
    assert_eq!(
        first.temp_dir().file_name().unwrap().to_str().unwrap(),
        "00000000-Tree-left"
    );
    assert_eq!(
        second.temp_dir().file_name().unwrap().to_str().unwrap(),
        "00000001-Tree-right"
    );
}

#[test]
fn workspace_allocation_writes_metadata_index_and_sanitized_paths() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let workspace = create_workspace(
        &layout,
        "Source Builder",
        Some("name / demo".to_string()),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();

    assert_eq!(
        workspace.log_dir().file_name().unwrap().to_str().unwrap(),
        "00000000-Source_Builder-name___demo"
    );
    assert!(workspace.raw_log_dir().is_dir());
    assert!(workspace.temp_dir().is_dir());
    assert!(workspace.log_dir().starts_with(layout.run_log_dir()));
    assert!(workspace.raw_log_dir().starts_with(workspace.log_dir()));
    assert!(workspace.temp_dir().starts_with(layout.run_tmp_dir()));
    assert!(!workspace.temp_dir().starts_with(workspace.log_dir()));
    assert_eq!(
        workspace.log_dir().file_name().unwrap(),
        workspace.temp_dir().file_name().unwrap()
    );
    let metadata: Value =
        serde_json::from_slice(&fs::read(workspace.log_dir().join("meta.json")).unwrap()).unwrap();
    assert_eq!(metadata["schema"], "bobr-workspace-v1");
    assert_eq!(metadata["serial"], 0);
    assert_eq!(metadata["tag"], "Source Builder");
    assert_eq!(metadata["recipe_name"], "name / demo");
    assert_eq!(
        metadata["build_key"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(
        metadata["temp_dir"],
        workspace.temp_dir().display().to_string()
    );

    let index = fs::read_to_string(layout.run_log_dir().join("index.jsonl")).unwrap();
    let records = index.lines().collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
    let record: Value = serde_json::from_str(records[0]).unwrap();
    assert_eq!(record["serial"], 0);
    assert_eq!(record["tag"], "Source Builder");
    assert_eq!(record["recipe_name"], "name / demo");
}

#[test]
fn store_temp_force_helpers_reject_paths_outside_store_tmp_root() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let old_log_tmp = layout.run_log_dir().join("00000000-Tree-demo").join("tmp");

    assert!(matches!(
        remove_store_temp_dir_force(&layout, &old_log_tmp),
        Err(StoreError::InvalidInput(message))
            if message.contains("must be under store temp root")
    ));

    let traversal = layout.run_tmp_dir().join("..").join("logs").join("oops");
    assert!(matches!(
        remove_store_temp_dir_force(&layout, &traversal),
        Err(StoreError::InvalidInput(message))
            if message.contains("must not contain '..'")
    ));
}

#[test]
fn quarantine_store_temp_moves_path_and_writes_metadata() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let workspace =
        create_workspace(&layout, "Tree", Some("demo".to_string()), "build-demo").unwrap();
    let original_temp_dir = workspace.temp_dir().to_path_buf();
    fs::write(original_temp_dir.join("scratch"), b"temp\n").unwrap();
    let build_key = compute_build_key("Tree", &json!({}), &[]).unwrap();

    let quarantined = quarantine_store_temp(
        &layout,
        StoreTempQuarantineRequest {
            temp_path: original_temp_dir.clone(),
            builder_tag: "Tree".to_string(),
            build_key,
            reason: "test quarantine".to_string(),
        },
    )
    .unwrap();

    assert!(!original_temp_dir.exists());
    assert!(
        quarantined
            .path
            .starts_with(layout.root().join(crate::store::QUARANTINE_DIR))
    );
    assert!(quarantined.path.join("scratch").is_file());
    let file_name = quarantined.path.file_name().unwrap().to_str().unwrap();
    let metadata_path = quarantined.path.with_file_name(format!("{file_name}.json"));
    let metadata: Value = serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
    assert_eq!(metadata["schema"], "bobr-quarantine-v1");
    assert_eq!(metadata["builder_tag"], "Tree");
    assert_eq!(metadata["build_key"], build_key.to_hex());
    assert_eq!(
        metadata["original_path"],
        original_temp_dir.display().to_string()
    );
    assert_eq!(
        metadata["quarantine_path"],
        quarantined.path.display().to_string()
    );
    assert_eq!(metadata["reason"], "test quarantine");
}

#[test]
fn parallel_workspace_allocation_does_not_reuse_serials() {
    let temp = tempdir().unwrap();
    let layout = Store::create(temp.path()).unwrap();
    let mut handles = Vec::new();

    for index in 0..8 {
        let layout = layout.clone();
        handles.push(thread::spawn(move || {
            create_workspace(
                &layout,
                "Tree",
                Some(format!("node-{index}")),
                format!("build-{index}"),
            )
            .unwrap()
            .log_dir()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
        }));
    }

    let names = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), 8);
    for serial in 0..8 {
        let prefix = format!("{serial:08}-Tree-node-");
        assert!(names.iter().any(|name| name.starts_with(&prefix)));
    }
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
    let first = publish_build(
        &layout,
        PublishRequest {
            publication_name: "plain".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello", "variant": "plain" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello", "variant": "plain" }),
                &[],
            ),
            created_at: sample_created_at().to_string(),
            staged_path: first_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    let exec_stage = temp.path().join("exec.txt");
    fs::write(&exec_stage, b"hello").unwrap();
    fs::set_permissions(&exec_stage, fs::Permissions::from_mode(0o755)).unwrap();
    let second = publish_build(
        &layout,
        PublishRequest {
            publication_name: "exec".to_string(),
            build_key: build_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello", "variant": "exec" }),
                &[],
            ),
            reuse_key: reuse_key_for(
                "CasTest",
                json!({ "kind": "sandbox-script", "source": "hello", "variant": "exec" }),
                &[],
            ),
            created_at: sample_created_at().to_string(),
            staged_path: exec_stage,
            inputs: vec![],
        },
    )
    .unwrap();

    assert_ne!(first.object_hash, second.object_hash);
    assert_ne!(first.build_key, second.build_key);
}

fn publish_text_output(
    layout: &Store,
    temp_root: &Path,
    publication_name: &str,
    text: &str,
) -> Publication {
    let stage = temp_root.join(format!("{publication_name}.txt"));
    fs::write(&stage, text.as_bytes()).unwrap();
    let payload = json!({
        "kind": "text-output",
        "name": publication_name,
        "text": text,
    });
    publish_build(
        layout,
        PublishRequest {
            publication_name: publication_name.to_string(),
            build_key: build_key_for("CasTest", payload.clone(), &[]),
            reuse_key: reuse_key_for("CasTest", payload, &[]),
            created_at: sample_created_at().to_string(),
            staged_path: stage,
            inputs: vec![],
        },
    )
    .unwrap()
}

fn parse_object_hash(value: &str) -> ObjectHash {
    ObjectHash::from_str(value).unwrap()
}

fn parse_build_key(value: &str) -> BuildKey {
    BuildKey::from_str(value).unwrap()
}

fn sample_created_at() -> &'static str {
    "2026-03-24T12:34:56.123456789Z"
}

fn build_key_for(builder_tag: &str, payload: Value, input_build_keys: &[BuildKey]) -> BuildKey {
    compute_build_key(builder_tag, &payload, input_build_keys).unwrap()
}

fn reuse_key_for(builder_tag: &str, payload: Value, inputs: &[ReuseInputIdentity]) -> ReuseKey {
    compute_reuse_key(builder_tag, &payload, inputs).unwrap()
}
