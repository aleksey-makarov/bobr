use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use sha2::Digest;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;



fn write_binary_request_json(path: &std::path::Path, name: &str, image: &str, digest: &str, source_url: &str, source_hash: &str) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "meta": { "name": name },
            "build": {
                "Binary": {
                    "kind": "binary-output",
                    "optimize": "size",
                    "image": {
                        "ContainerImage": {
                            "image": image,
                            "digest": digest
                        }
                    },
                    "script": {
                        "Text": {
                            "kind": "build-script",
                            "source": "#!/bin/sh\nexit 0\n"
                        }
                    },
                    "sources": [
                        {
                            "Fetch": {
                                "url": source_url,
                                "hash": source_hash,
                                "unpack": true
                            }
                        }
                    ]
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

fn write_container_image_request_json(path: &std::path::Path, name: &str, image: &str, digest: &str) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
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

fn install_fake_podman(dir: &std::path::Path, inspect_json: &str) -> std::path::PathBuf {
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
    script_path
}

fn write_request_json(path: &std::path::Path, name: &str, kind: &str, source: &str) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
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

#[test]
fn cli_uses_default_dot_mbuild_request_json() {
    let workspace = tempdir().unwrap();
    fs::create_dir_all(workspace.path().join(".mbuild")).unwrap();
    write_request_json(
        &workspace.path().join(".mbuild").join("request.json"),
        "default-request",
        "plain-text",
        "hello default",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("build_key: sha256:"));
    assert!(stdout.contains("object_hash: sha256:"));
    assert!(stdout.contains("object_path:"));
}

#[test]
fn cli_accepts_explicit_request_json_path() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("custom.json");
    write_request_json(&request_path, "custom-request", "plain-text", "hello custom");

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("build_key: sha256:"));
    assert!(stdout.contains("object_hash: sha256:"));
}

#[test]
fn cli_executes_container_image_request_with_fake_podman() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("container-image.json");
    write_container_image_request_json(
        &request_path,
        "bootstrap-image",
        "docker.io/library/buildpack-deps:bookworm",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );

    let fake_bin_dir = workspace.path().join("fake-bin");
    fs::create_dir_all(&fake_bin_dir).unwrap();
    install_fake_podman(
        &fake_bin_dir,
        r#"[{"Id":"sha256:imageid","RepoDigests":["docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}]"#,
    );

    let path_value = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut joined = fake_bin_dir.as_os_str().to_os_string();
            joined.push(":");
            joined.push(existing);
            joined
        }
        None => fake_bin_dir.as_os_str().to_os_string(),
    };

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&request_path)
        .env("PATH", path_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);

    let refs_dir = workspace.path().join(".mbuild").join("object-refs");
    let published_path = fs::read_link(refs_dir.join("bootstrap-image")).unwrap();
    assert!(published_path.to_string_lossy().contains("../objects/sha256:"));
}

#[test]
fn cli_rejects_container_image_request_when_digest_does_not_match() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("container-image-bad.json");
    write_container_image_request_json(
        &request_path,
        "bootstrap-image",
        "docker.io/library/buildpack-deps:bookworm",
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );

    let fake_bin_dir = workspace.path().join("fake-bin");
    fs::create_dir_all(&fake_bin_dir).unwrap();
    install_fake_podman(
        &fake_bin_dir,
        r#"[{"Id":"sha256:imageid","RepoDigests":["docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}]"#,
    );

    let path_value = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut joined = fake_bin_dir.as_os_str().to_os_string();
            joined.push(":");
            joined.push(existing);
            joined
        }
        None => fake_bin_dir.as_os_str().to_os_string(),
    };

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&request_path)
        .env("PATH", path_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]:"), "{stderr}");
    assert!(stderr.contains("does not match required digest"), "{stderr}");
}

#[test]
fn cli_executes_binary_request_with_fake_podman_and_nested_inputs() {
    let workspace = tempdir().unwrap();
    let source_tar = {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello binary\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/README.txt").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &body[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    };

    let (url, handle) = match spawn_http_server(source_tar.clone(), "application/gzip") {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("failed to start test HTTP server: {error}"),
    };
    let source_hash = format!("sha256:{:x}", sha2::Sha256::digest(&source_tar));

    let request_path = workspace.path().join("binary.json");
    write_binary_request_json(
        &request_path,
        "zstd-bin",
        "docker.io/library/buildpack-deps:bookworm",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        &url,
        &source_hash,
    );

    let fake_bin_dir = workspace.path().join("fake-bin");
    fs::create_dir_all(&fake_bin_dir).unwrap();
    install_fake_podman(
        &fake_bin_dir,
        r#"[{"Id":"sha256:imageid","RepoDigests":["docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}]"#,
    );

    let path_value = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut joined = fake_bin_dir.as_os_str().to_os_string();
            joined.push(":");
            joined.push(existing);
            joined
        }
        None => fake_bin_dir.as_os_str().to_os_string(),
    };

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&request_path)
        .env("PATH", path_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();
    handle.join().unwrap();

    assert!(output.status.success(), "{:?}", output);

    let object_path = workspace.path().join(".mbuild").join("object-refs").join("zstd-bin");
    let resolved = workspace.path().join(".mbuild").join("object-refs").join(fs::read_link(&object_path).unwrap());
    let object_dir = fs::canonicalize(resolved).unwrap();
    assert_eq!(fs::read_to_string(object_dir.join("copied").join("README.txt")).unwrap(), "hello binary\n");
    assert_eq!(fs::read_to_string(object_dir.join("image-ref.txt")).unwrap(), "docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n");
}
