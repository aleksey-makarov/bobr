#![allow(missing_docs)]
mod support;

use bobr::{ExecutionError, Request, execute, realize};
use bobr_core::{CancellationToken, ObjectHash};
use bobr_source::fetch::{FetchRequest, Limits, SourceEntry, run_fetch};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use support::{
    base_image_recipe, build_key_for_object, group_recipe, recipe_node, remove_build_ref,
    spawn_test_oci_registry, store_root, tree_file_recipe, with_fresh_run, write_request,
};
use tempfile::{TempDir, tempdir};

#[derive(Debug, PartialEq, Eq)]
struct StoreSnapshot {
    objects: Vec<String>,
    fs_files: Vec<String>,
    builds: BTreeMap<String, PathBuf>,
    reuses: BTreeMap<String, PathBuf>,
    object_refs: BTreeMap<String, PathBuf>,
    object_records: BTreeMap<String, serde_json::Value>,
}

fn snapshot(root: &Path) -> StoreSnapshot {
    let store = store_root(root);
    StoreSnapshot {
        objects: names_in(&store.join("objects")),
        fs_files: relative_files(&store.join("fs-files")),
        builds: links_in(&store.join("builds")),
        reuses: links_in(&store.join("reuses")),
        object_refs: links_in(&store.join("object-refs")),
        object_records: normalized_records_in(&store.join("object-records")),
    }
}

fn names_in(path: &Path) -> Vec<String> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn relative_files(root: &Path) -> Vec<String> {
    fn visit(root: &Path, path: &Path, result: &mut Vec<String>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, result);
            } else {
                result.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut result = Vec::new();
    visit(root, root, &mut result);
    result.sort();
    result
}

fn links_in(path: &Path) -> BTreeMap<String, PathBuf> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read_link(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn normalized_records_in(path: &Path) -> BTreeMap<String, serde_json::Value> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let mut record: serde_json::Value =
                serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
            record
                .as_object_mut()
                .expect("object record must be a JSON object")
                .remove("run_id");
            (entry.file_name().to_string_lossy().into_owned(), record)
        })
        .collect()
}

fn request_path(environment: &TempDir, recipe: &serde_json::Value) -> PathBuf {
    let path = environment.path().join("request.json");
    write_request(&path, recipe);
    path
}

fn parse_fresh(path: &Path) -> Request {
    let bytes = fs::read(path).unwrap();
    Request::parse_json(&with_fresh_run(&bytes)).unwrap()
}

fn run_legacy(path: &Path) -> Result<ObjectHash, ExecutionError> {
    execute(parse_fresh(path), CancellationToken::new())
}

fn run_dynamic(path: &Path) -> Result<ObjectHash, ExecutionError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(realize(parse_fresh(path), CancellationToken::new()))
        .map(|goals| goals[0].object_hash)
}

fn assert_stores_equal(left: &TempDir, right: &TempDir) {
    assert_eq!(snapshot(left.path()), snapshot(right.path()));
    let left_store = bobr_store::Store::create(&store_root(left.path())).unwrap();
    let right_store = bobr_store::Store::create(&store_root(right.path())).unwrap();
    for hash in snapshot(left.path()).objects {
        let hash: ObjectHash = hash.parse().unwrap();
        assert_eq!(
            fsobj_hash::hash_path(left_store.object_path(hash).unwrap().unwrap()).unwrap(),
            fsobj_hash::hash_path(right_store.object_path(hash).unwrap().unwrap()).unwrap(),
        );
    }
}

fn spawn_http_server(
    body: Vec<u8>,
    expected_requests: usize,
) -> std::io::Result<(String, thread::JoinHandle<()>)> {
    let listener = (0..10)
        .find_map(|attempt| match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => Some(Ok(listener)),
            Err(error)
                if attempt < 9
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrInUse
                    ) =>
            {
                thread::sleep(Duration::from_millis(10));
                None
            }
            Err(error) => Some(Err(error)),
        })
        .unwrap_or_else(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "failed to bind test HTTP listener",
            ))
        })?;
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/payload");
    let handle = thread::spawn(move || {
        for _ in 0..expected_requests {
            let (mut stream, _) = listener.accept().unwrap();
            drain_request(&mut stream);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        }
    });
    Ok((url, handle))
}

fn drain_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
    }
}

#[test]
fn cold_and_warm_tree_group_builds_match() {
    let legacy = tempdir().unwrap();
    let dynamic = tempdir().unwrap();
    let recipe = recipe_node(
        "root",
        "Group",
        json!({}),
        json!({
            "a": tree_file_recipe("a", "a.txt", "alpha", false),
            "b": tree_file_recipe("b", "b.txt", "beta", false),
        }),
    );
    let legacy_request = request_path(&legacy, &recipe);
    let dynamic_request = request_path(&dynamic, &recipe);

    let legacy_cold = run_legacy(&legacy_request).unwrap();
    let dynamic_cold = run_dynamic(&dynamic_request).unwrap();
    assert_eq!(legacy_cold, dynamic_cold);
    assert_stores_equal(&legacy, &dynamic);

    let legacy_warm = run_legacy(&legacy_request).unwrap();
    let dynamic_warm = run_dynamic(&dynamic_request).unwrap();
    assert_eq!(legacy_warm, legacy_cold);
    assert_eq!(dynamic_warm, dynamic_cold);
    assert_stores_equal(&legacy, &dynamic);
}

#[test]
fn path_source_and_parent_builder_match() {
    let source_dir = tempdir().unwrap();
    let source_path = source_dir.path().join("source.txt");
    fs::write(&source_path, b"source parity\n").unwrap();
    let source_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let recipe = group_recipe(
        "root",
        vec![json!({
            "name": "source",
            "tag": "Source",
            "object_hash": source_hash,
            "origin": {
                "tag": "Path",
                "path": source_path,
                "unpack": false,
            }
        })],
    );
    let legacy = tempdir().unwrap();
    let dynamic = tempdir().unwrap();
    let legacy_request = request_path(&legacy, &recipe);
    let dynamic_request = request_path(&dynamic, &recipe);

    assert_eq!(
        run_legacy(&legacy_request).unwrap(),
        run_dynamic(&dynamic_request).unwrap()
    );
    assert_stores_equal(&legacy, &dynamic);
}

#[test]
fn missing_source_failure_class_matches() {
    let hash = "1".repeat(64);
    let recipe = json!({
        "name": "source",
        "tag": "Source",
        "object_hash": hash,
    });
    let legacy = tempdir().unwrap();
    let dynamic = tempdir().unwrap();
    let legacy_error = run_legacy(&request_path(&legacy, &recipe)).unwrap_err();
    let dynamic_error = run_dynamic(&request_path(&dynamic, &recipe)).unwrap_err();

    assert!(matches!(legacy_error, ExecutionError::Build(_)));
    assert!(matches!(dynamic_error, ExecutionError::Build(_)));
    assert!(legacy_error.to_string().contains("has no origin"));
    assert!(dynamic_error.to_string().contains("has no origin"));
    assert_eq!(snapshot(legacy.path()), snapshot(dynamic.path()));
}

#[test]
fn flat_fetch_and_source_goal_match_for_path_origin() {
    let source_dir = tempdir().unwrap();
    let source_path = source_dir.path().join("source.txt");
    fs::write(&source_path, b"fetch parity\n").unwrap();
    let hash = fsobj_hash::hash_path(&source_path).unwrap();
    let legacy = tempdir().unwrap();
    let dynamic = tempdir().unwrap();
    let legacy_store = store_root(legacy.path());
    fs::create_dir_all(&legacy_store).unwrap();
    let legacy_logs = legacy.path().join("logs");
    let legacy_work = legacy.path().join("work");
    fs::create_dir(&legacy_logs).unwrap();
    fs::create_dir(&legacy_work).unwrap();
    let fetch_request = FetchRequest {
        schema: bobr_source::fetch::FETCH_REQUEST_SCHEMA.to_string(),
        store: legacy_store,
        logs: legacy_logs,
        work: legacy_work,
        run_id: "legacy-fetch".to_string(),
        limits: Limits::default(),
        quiet: Some(true),
        sources: vec![SourceEntry {
            name: "source".to_string(),
            object_hash: hash.to_string(),
            origin: Some(json!({
                "tag": "Path",
                "path": source_path,
                "unpack": false,
            })),
        }],
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let summary = runtime.block_on(run_fetch(fetch_request)).unwrap();
    assert!(summary.is_success());

    let recipe = json!({
        "name": "source",
        "tag": "Source",
        "object_hash": hash,
        "origin": {
            "tag": "Path",
            "path": source_path,
            "unpack": false,
        }
    });
    assert_eq!(run_dynamic(&request_path(&dynamic, &recipe)).unwrap(), hash);
    assert_stores_equal(&legacy, &dynamic);
}

#[test]
fn reuse_after_missing_build_mapping_matches() {
    let recipe = tree_file_recipe("hello", "hello.txt", "reuse parity\n", false);
    let legacy = tempdir().unwrap();
    let dynamic = tempdir().unwrap();
    let legacy_request = request_path(&legacy, &recipe);
    let dynamic_request = request_path(&dynamic, &recipe);

    let legacy_hash = run_legacy(&legacy_request).unwrap();
    let dynamic_hash = run_dynamic(&dynamic_request).unwrap();
    assert_eq!(legacy_hash, dynamic_hash);
    remove_build_ref(
        legacy.path(),
        build_key_for_object(legacy.path(), legacy_hash),
    );
    remove_build_ref(
        dynamic.path(),
        build_key_for_object(dynamic.path(), dynamic_hash),
    );

    assert_eq!(
        run_legacy(&legacy_request).unwrap(),
        run_dynamic(&dynamic_request).unwrap()
    );
    assert_stores_equal(&legacy, &dynamic);
}

#[test]
fn builder_failure_class_matches() {
    let recipe = group_recipe("empty-group", Vec::new());
    let legacy = tempdir().unwrap();
    let dynamic = tempdir().unwrap();

    let legacy_error = run_legacy(&request_path(&legacy, &recipe)).unwrap_err();
    let dynamic_error = run_dynamic(&request_path(&dynamic, &recipe)).unwrap_err();
    assert!(matches!(legacy_error, ExecutionError::Build(_)));
    assert!(matches!(dynamic_error, ExecutionError::Build(_)));
    assert!(legacy_error.to_string().contains("at least one input"));
    assert!(dynamic_error.to_string().contains("at least one input"));
    assert_eq!(snapshot(legacy.path()), snapshot(dynamic.path()));
}

#[test]
fn path_hash_mismatch_matches() {
    let source_dir = tempdir().unwrap();
    let source_path = source_dir.path().join("source.txt");
    fs::write(&source_path, b"wrong declared hash\n").unwrap();
    let actual_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let wrong_hash = "1".repeat(64);
    let recipe = json!({
        "name": "source",
        "tag": "Source",
        "object_hash": wrong_hash,
        "origin": {
            "tag": "Path",
            "path": source_path,
            "unpack": false,
        }
    });
    let legacy = tempdir().unwrap();
    let dynamic = tempdir().unwrap();

    let legacy_error = run_legacy(&request_path(&legacy, &recipe)).unwrap_err();
    let dynamic_error = run_dynamic(&request_path(&dynamic, &recipe)).unwrap_err();
    assert!(legacy_error.to_string().contains(&actual_hash.to_string()));
    assert!(dynamic_error.to_string().contains(&actual_hash.to_string()));
    assert_stores_equal(&legacy, &dynamic);
}

#[test]
fn http_source_goal_matches_flat_fetch() {
    let body = b"http fetch parity\n".to_vec();
    let source_file = tempfile::NamedTempFile::new().unwrap();
    fs::write(source_file.path(), &body).unwrap();
    let hash = fsobj_hash::hash_path(source_file.path()).unwrap();
    let (url, server) = match spawn_http_server(body, 2) {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("failed to start test HTTP server: {error}"),
    };
    let origin = json!({
        "tag": "Http",
        "url": url,
        "unpack": false,
    });
    let legacy = tempdir().unwrap();
    let legacy_store = store_root(legacy.path());
    fs::create_dir_all(&legacy_store).unwrap();
    let legacy_logs = legacy.path().join("logs");
    let legacy_work = legacy.path().join("work");
    fs::create_dir(&legacy_logs).unwrap();
    fs::create_dir(&legacy_work).unwrap();
    let request = FetchRequest {
        schema: bobr_source::fetch::FETCH_REQUEST_SCHEMA.to_string(),
        store: legacy_store,
        logs: legacy_logs,
        work: legacy_work,
        run_id: "legacy-fetch".to_string(),
        limits: Limits::default(),
        quiet: Some(true),
        sources: vec![SourceEntry {
            name: "source".to_string(),
            object_hash: hash.to_string(),
            origin: Some(origin.clone()),
        }],
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(runtime.block_on(run_fetch(request)).unwrap().is_success());

    let dynamic = tempdir().unwrap();
    let recipe = json!({
        "name": "source",
        "tag": "Source",
        "object_hash": hash,
        "origin": origin,
    });
    assert_eq!(run_dynamic(&request_path(&dynamic, &recipe)).unwrap(), hash);
    server.join().unwrap();
    assert_stores_equal(&legacy, &dynamic);
}

#[test]
fn oci_source_goal_matches_flat_fetch() {
    let (_server, image, digest, hash) = spawn_test_oci_registry();
    let legacy = tempdir().unwrap();
    let legacy_store = store_root(legacy.path());
    fs::create_dir_all(&legacy_store).unwrap();
    let legacy_logs = legacy.path().join("logs");
    let legacy_work = legacy.path().join("work");
    fs::create_dir(&legacy_logs).unwrap();
    fs::create_dir(&legacy_work).unwrap();
    let recipe = base_image_recipe(&image, &digest, &hash);
    let origin = recipe.get("origin").unwrap().clone();
    let request = FetchRequest {
        schema: bobr_source::fetch::FETCH_REQUEST_SCHEMA.to_string(),
        store: legacy_store,
        logs: legacy_logs,
        work: legacy_work,
        run_id: "legacy-fetch".to_string(),
        limits: Limits::default(),
        quiet: Some(true),
        sources: vec![SourceEntry {
            name: "base-image".to_string(),
            object_hash: hash.clone(),
            origin: Some(origin),
        }],
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(runtime.block_on(run_fetch(request)).unwrap().is_success());

    let dynamic = tempdir().unwrap();
    assert_eq!(
        run_dynamic(&request_path(&dynamic, &recipe))
            .unwrap()
            .to_string(),
        hash
    );
    assert_stores_equal(&legacy, &dynamic);
}
