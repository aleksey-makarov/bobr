#![allow(missing_docs)]
mod support;

use bobr::{ExecutionError, Request, realize};
use bobr_core::{CancellationToken, ObjectHash};
use bobr_store::{Store, load_build_handle};
use serde_json::json;
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

fn request_path(environment: &TempDir, recipe: &serde_json::Value) -> PathBuf {
    let path = environment.path().join("request.json");
    write_request(&path, recipe);
    path
}

fn parse_fresh(path: &Path) -> Request {
    let bytes = fs::read(path).unwrap();
    Request::parse_json(&with_fresh_run(&bytes)).unwrap()
}

fn run_realizer(path: &Path) -> Result<ObjectHash, ExecutionError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(realize(parse_fresh(path), CancellationToken::new()))
        .map(|goals| goals[0].object_hash)
}

fn object_names(environment: &TempDir) -> Vec<String> {
    let mut names = fs::read_dir(store_root(environment.path()).join("objects"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
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
fn cold_and_warm_tree_group_produce_one_stable_goal() {
    let environment = tempdir().unwrap();
    let recipe = recipe_node(
        "root",
        "Group",
        json!({}),
        json!({
            "a": tree_file_recipe("a", "a.txt", "alpha", false),
            "b": tree_file_recipe("b", "b.txt", "beta", false),
        }),
    );
    let request = request_path(&environment, &recipe);

    let cold = run_realizer(&request).unwrap();
    let cold_objects = object_names(&environment);
    let warm = run_realizer(&request).unwrap();

    assert_eq!(warm, cold);
    assert_eq!(object_names(&environment), cold_objects);
}

#[test]
fn path_source_and_parent_builder_publish_complete_content() {
    let source_dir = tempdir().unwrap();
    let source_path = source_dir.path().join("source.txt");
    fs::write(&source_path, b"source regression\n").unwrap();
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
    let environment = tempdir().unwrap();

    let goal = run_realizer(&request_path(&environment, &recipe)).unwrap();
    let store = Store::create(&store_root(environment.path())).unwrap();

    assert!(store.object_path(source_hash).unwrap().is_some());
    assert!(store.object_path(goal).unwrap().is_some());
}

#[test]
fn unavailable_source_without_origin_is_a_build_error() {
    let environment = tempdir().unwrap();
    let recipe = json!({
        "name": "source",
        "tag": "Source",
        "object_hash": "1".repeat(64),
    });

    let error = run_realizer(&request_path(&environment, &recipe)).unwrap_err();

    assert!(matches!(error, ExecutionError::Build(_)));
    assert!(error.to_string().contains("has no origin"));
    assert!(object_names(&environment).is_empty());
}

#[test]
fn path_source_can_be_a_goal() {
    let source_dir = tempdir().unwrap();
    let source_path = source_dir.path().join("source.txt");
    fs::write(&source_path, b"source goal\n").unwrap();
    let hash = fsobj_hash::hash_path(&source_path).unwrap();
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
    let environment = tempdir().unwrap();

    assert_eq!(
        run_realizer(&request_path(&environment, &recipe)).unwrap(),
        hash
    );
}

#[test]
fn reuse_mapping_restores_a_missing_build_mapping() {
    let environment = tempdir().unwrap();
    let recipe = tree_file_recipe("hello", "hello.txt", "reuse regression\n", false);
    let request = request_path(&environment, &recipe);

    let first = run_realizer(&request).unwrap();
    let build_key = build_key_for_object(environment.path(), first);
    remove_build_ref(environment.path(), build_key);
    let second = run_realizer(&request).unwrap();
    let store = Store::create(&store_root(environment.path())).unwrap();

    assert_eq!(second, first);
    assert_eq!(load_build_handle(&store, build_key).unwrap(), Some(first));
}

#[test]
fn invalid_builder_configuration_is_a_build_error() {
    let environment = tempdir().unwrap();
    let recipe = group_recipe("empty-group", Vec::new());

    let error = run_realizer(&request_path(&environment, &recipe)).unwrap_err();

    assert!(matches!(error, ExecutionError::Build(_)));
    assert!(error.to_string().contains("at least one input"));
}

#[test]
fn path_hash_mismatch_names_and_imports_the_actual_object() {
    let source_dir = tempdir().unwrap();
    let source_path = source_dir.path().join("source.txt");
    fs::write(&source_path, b"wrong declared hash\n").unwrap();
    let actual_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let declared: ObjectHash = "1".repeat(64).parse().unwrap();
    let recipe = json!({
        "name": "source",
        "tag": "Source",
        "object_hash": declared,
        "origin": {
            "tag": "Path",
            "path": source_path,
            "unpack": false,
        }
    });
    let environment = tempdir().unwrap();

    let error = run_realizer(&request_path(&environment, &recipe)).unwrap_err();
    let store = Store::create(&store_root(environment.path())).unwrap();

    assert!(error.to_string().contains(&actual_hash.to_string()));
    assert!(store.object_path(actual_hash).unwrap().is_some());
    assert!(store.object_path(declared).unwrap().is_none());
}

#[test]
fn http_source_goal_uses_the_async_acquisition_engine() {
    let body = b"http acquisition regression\n".to_vec();
    let source_file = tempfile::NamedTempFile::new().unwrap();
    fs::write(source_file.path(), &body).unwrap();
    let hash = fsobj_hash::hash_path(source_file.path()).unwrap();
    let (url, server) = match spawn_http_server(body, 1) {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("failed to start test HTTP server: {error}"),
    };
    let recipe = json!({
        "name": "source",
        "tag": "Source",
        "object_hash": hash,
        "origin": {
            "tag": "Http",
            "url": url,
            "unpack": false,
        },
    });
    let environment = tempdir().unwrap();

    assert_eq!(
        run_realizer(&request_path(&environment, &recipe)).unwrap(),
        hash
    );
    server.join().unwrap();
}

#[test]
fn oci_source_goal_uses_the_async_acquisition_engine() {
    let (_server, image, digest, hash) = spawn_test_oci_registry();
    let recipe = base_image_recipe(&image, &digest, &hash);
    let environment = tempdir().unwrap();

    assert_eq!(
        run_realizer(&request_path(&environment, &recipe))
            .unwrap()
            .to_string(),
        hash
    );
}
