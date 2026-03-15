use mbuild::runtime::{load_build_request, run_request_in_workspace, run_workspace_build};
use mbuild_core::BuildRequest;
use mbuild_core::builder::BuildMeta;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use std::sync::{Mutex, OnceLock};
use tempfile::tempdir;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn install_fake_podman(dir: &Path, inspect_json: &str) {
    let script_path = dir.join("podman");
    fs::write(
        &script_path,
        format!(
            "#!/usr/bin/env bash\nset -euo pipefail\nif [ \"$1\" = image ] && [ \"$2\" = inspect ]; then\n  cat <<'JSON'\n{inspect_json}\nJSON\nelse\n  echo unexpected podman invocation: \"$@\" >&2\n  exit 1\nfi\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
    }
}

fn with_fake_podman<T>(inspect_json: &str, f: impl FnOnce() -> T) -> T {
    let _guard = env_lock().lock().unwrap();
    let temp = tempdir().unwrap();
    install_fake_podman(temp.path(), inspect_json);
    let previous_path = env::var_os("PATH");
    let new_path = match &previous_path {
        Some(existing) => {
            let mut joined = temp.path().as_os_str().to_os_string();
            joined.push(":");
            joined.push(existing);
            joined
        }
        None => temp.path().as_os_str().to_os_string(),
    };
    unsafe { env::set_var("PATH", &new_path) };
    let result = f();
    match previous_path {
        Some(path) => unsafe { env::set_var("PATH", path) },
        None => unsafe { env::remove_var("PATH") },
    }
    result
}


fn text_request(name: &str, kind: &str, source: &str) -> BuildRequest {
    BuildRequest {
        meta: BuildMeta {
            name: name.to_string(),
            extra: Map::new(),
        },
        build: json!({
            "Text": {
                "kind": kind,
                "source": source,
            }
        }),
    }
}

fn build_path(root: &Path, build_key: impl ToString) -> PathBuf {
    root.join(".mbuild")
        .join("builds")
        .join(format!("{}.json", build_key.to_string()))
}

fn write_request_json(path: &Path, name: &str, kind: &str, source: &str) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "meta": { "name": name },
            "build": {
                "Text": {
                    "kind": kind,
                    "source": source,
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_container_image_request_json(path: &Path, name: &str, image: &str, digest: &str) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "meta": { "name": name },
            "build": {
                "ContainerImage": {
                    "image": image,
                    "digest": digest
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_fetch_request_json(path: &Path, name: &str, url: &str, hash: &str, unpack: bool) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "meta": { "name": name },
            "build": {
                "Fetch": {
                    "url": url,
                    "hash": hash,
                    "unpack": unpack
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn spawn_http_server(body: Vec<u8>, content_type: &'static str) -> Result<(String, thread::JoinHandle<()>), std::io::Error> {
    let listener = (0..10)
        .find_map(|attempt| match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => Some(Ok(listener)),
            Err(error)
                if attempt < 9
                    && matches!(error.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrInUse) =>
            {
                thread::sleep(Duration::from_millis(10));
                None
            }
            Err(error) => Some(Err(error)),
        })
        .unwrap_or_else(|| Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "failed to bind test HTTP listener")))?;
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}/payload", addr);
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        drain_request(&mut stream);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
            body.len(),
            content_type
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        stream.flush().unwrap();
    });
    Ok((url, handle))
}

fn drain_request(stream: &mut TcpStream) {
    let mut buf = [0u8; 1024];
    let mut request = Vec::new();
    loop {
        let read = stream.read(&mut buf).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buf[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
}

#[test]
fn text_request_creates_store_entries_and_refs() {
    let workspace = tempdir().unwrap();
    let request = text_request("hello", "build-script", "#!/bin/sh\necho hi\n");

    let published = run_request_in_workspace(workspace.path(), &request).unwrap();

    assert!(published.object_path.exists());
    assert_eq!(
        fs::read_to_string(&published.object_path).unwrap(),
        "#!/bin/sh\necho hi\n"
    );
    #[cfg(unix)]
    {
        let mode = fs::metadata(&published.object_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    let build_file = build_path(workspace.path(), published.record.build_key);
    assert!(build_file.exists());
    let build_json: Value = serde_json::from_slice(&fs::read(&build_file).unwrap()).unwrap();
    assert_eq!(build_json["kind"], Value::String("build-script".to_string()));
    assert_eq!(
        build_json["producer"]["builder"],
        Value::String("text".to_string())
    );
    assert_eq!(
        build_json["object_hash"],
        Value::String(published.record.object_hash.to_string())
    );

    assert_eq!(
        fs::read_link(workspace.path().join(".mbuild").join("meta-refs").join("hello.json")).unwrap(),
        PathBuf::from("..")
            .join("builds")
            .join(format!("{}.json", published.record.build_key))
    );
    assert_eq!(
        fs::read_link(workspace.path().join(".mbuild").join("object-refs").join("hello")).unwrap(),
        PathBuf::from("..")
            .join("objects")
            .join(published.record.object_hash.to_string())
    );
}

#[test]
fn repeated_text_request_reuses_build_record_and_object() {
    let workspace = tempdir().unwrap();
    let request = text_request("hello", "plain-text", "hello");

    let first = run_request_in_workspace(workspace.path(), &request).unwrap();
    let second = run_request_in_workspace(workspace.path(), &request).unwrap();

    assert_eq!(first.record.build_key, second.record.build_key);
    assert_eq!(first.record.object_hash, second.record.object_hash);

    let objects_dir = workspace.path().join(".mbuild").join("objects");
    let builds_dir = workspace.path().join(".mbuild").join("builds");
    assert_eq!(fs::read_dir(objects_dir).unwrap().count(), 1);
    assert_eq!(fs::read_dir(builds_dir).unwrap().count(), 1);
}

#[test]
fn workspace_build_reads_request_json_from_explicit_path() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("request.json");
    write_request_json(&request_path, "script", "build-script", "#!/bin/sh\necho explicit\n");

    let published = run_workspace_build(workspace.path(), &request_path).unwrap();

    assert_eq!(
        fs::read_to_string(&published.object_path).unwrap(),
        "#!/bin/sh\necho explicit\n"
    );
    assert!(build_path(workspace.path(), published.record.build_key).exists());
}

#[test]
fn workspace_build_executes_fetch_request_end_to_end() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("fetch-request.json");
    let payload = b"hello fetch\n".to_vec();
    let hash = format!("sha256:{:x}", Sha256::digest(&payload));
    let (url, handle) = match spawn_http_server(payload.clone(), "application/octet-stream") {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping fetch runtime test because TCP bind is not permitted in this environment: {error}");
            return;
        }
        Err(error) => panic!("failed to start test HTTP server: {error}"),
    };
    write_fetch_request_json(&request_path, "fetched-file", &url, &hash, false);

    let published = run_workspace_build(workspace.path(), &request_path).unwrap();
    handle.join().unwrap();

    assert_eq!(published.record.kind, "fetched-file");
    assert_eq!(published.record.producer.builder, "fetch");
    assert_eq!(fs::read(&published.object_path).unwrap(), payload);
    assert_eq!(published.record.attrs["source_url"], Value::String(url));
    assert_eq!(published.record.attrs["declared_hash"], Value::String(hash));
    assert_eq!(published.record.attrs["unpack"], Value::Bool(false));
}

#[test]
fn workspace_build_executes_container_image_request_and_persists_full_record() {
    with_fake_podman(
        r#"[{"Id":"sha256:imageid","RepoDigests":["docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}]"#,
        || {
            let workspace = tempdir().unwrap();
            let request_path = workspace.path().join("container-image-request.json");
            write_container_image_request_json(
                &request_path,
                "bootstrap-image",
                "docker.io/library/buildpack-deps:bookworm",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            );

            let published = run_workspace_build(workspace.path(), &request_path).unwrap();

            assert_eq!(published.record.kind, "container-image");
            assert_eq!(published.record.producer.builder, "container-image");
            assert_eq!(published.record.attrs["image"], Value::String("docker.io/library/buildpack-deps:bookworm".to_string()));
            assert_eq!(published.record.attrs["image_ref"], Value::String("docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()));
            assert_eq!(published.record.attrs["image_id"], Value::String("sha256:imageid".to_string()));
            assert_eq!(published.record.attrs["image_digest"], Value::String("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()));

            let descriptor: Value = serde_json::from_slice(&fs::read(&published.object_path).unwrap()).unwrap();
            assert_eq!(descriptor["schema"], Value::String("mbuild-container-image-object-v1".to_string()));
            assert_eq!(descriptor["storage"], Value::String("external-podman".to_string()));
            assert_eq!(descriptor["image_ref"], Value::String("docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()));
            assert_eq!(descriptor["image_digest"], Value::String("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()));

            let build_file = build_path(workspace.path(), published.record.build_key);
            let build_json: Value = serde_json::from_slice(&fs::read(&build_file).unwrap()).unwrap();
            assert_eq!(build_json["kind"], Value::String("container-image".to_string()));
            assert_eq!(build_json["producer"]["builder"], Value::String("container-image".to_string()));
            assert_eq!(build_json["object_hash"], Value::String(published.record.object_hash.to_string()));
            assert_eq!(build_json["attrs"]["image"], Value::String("docker.io/library/buildpack-deps:bookworm".to_string()));
            assert_eq!(build_json["attrs"]["image_ref"], Value::String("docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()));
            assert_eq!(build_json["attrs"]["image_id"], Value::String("sha256:imageid".to_string()));
            assert_eq!(build_json["attrs"]["image_digest"], Value::String("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()));
        },
    );
}

#[test]
fn load_build_request_reads_modern_json_shape() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("request.json");
    write_request_json(&request_path, "script", "plain-text", "hello");

    let request = load_build_request(&request_path).unwrap();

    assert_eq!(request.meta.name, "script");
    assert_eq!(
        request.build,
        json!({
            "Text": {
                "kind": "plain-text",
                "source": "hello",
            }
        })
    );
}



