mod support;

use mbuild::recipe_runtime::{
    BuildRunOptions, run_recipe_json_in_workspace, run_recipe_json_in_workspace_with_options,
};
use mbuild_core::{StoreLayout, load_build_handle};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use std::time::Instant;
use support::{
    base_image_recipe, binary_recipe, build_ref_path, image_recipe, recipe_node, script_recipe,
    source_recipe, spawn_test_oci_registry, write_recipe,
};
use tempfile::tempdir;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn install_fake_podman(dir: &Path) {
    let script_path = dir.join("podman");
    let script = include_str!("assets/fake_podman_full.sh");
    fs::write(&script_path, script).unwrap();
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
    }
}

fn with_fake_podman<T>(f: impl FnOnce() -> T) -> T {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempdir().unwrap();
    install_fake_podman(temp.path());
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

fn spawn_http_server(
    body: Vec<u8>,
    content_type: &'static str,
) -> Result<(String, thread::JoinHandle<()>), std::io::Error> {
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

fn spawn_barrier_http_server(
    body: Vec<u8>,
    content_type: &'static str,
    expected_requests: usize,
    timeout: Duration,
) -> Result<(String, thread::JoinHandle<()>), std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}/payload", addr);
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        let mut streams = Vec::new();
        while streams.len() < expected_requests && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    drain_request(&mut stream);
                    streams.push(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to accept barrier HTTP request: {error}"),
            }
        }

        let (status_line, payload) = if streams.len() == expected_requests {
            ("HTTP/1.1 200 OK", Some((body, content_type)))
        } else {
            ("HTTP/1.1 503 Service Unavailable", None)
        };

        for mut stream in streams {
            match &payload {
                Some((body, content_type)) => {
                    let response = format!(
                        "{status_line}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        content_type
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.write_all(body).unwrap();
                }
                None => {
                    let response =
                        format!("{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
            stream.flush().unwrap();
        }
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

fn make_full_recipe(url: &str, source_hash: &str, image: &str, digest: &str) -> Value {
    image_recipe(
        "final-image",
        vec![binary_recipe("binary", url, source_hash, image, digest)],
    )
}

fn binary_with_two_sources_recipe(
    url_a: &str,
    url_b: &str,
    source_hash: &str,
    image: &str,
    digest: &str,
) -> Value {
    recipe_node(
        "binary",
        "Binary",
        json!({
            "kind": "binary-output",
            "optimize": "size"
        }),
        json!({
            "image": base_image_recipe(image, digest),
            "script": script_recipe(),
            "sources": [
                source_recipe(url_a, source_hash),
                source_recipe(url_b, source_hash)
            ]
        }),
    )
}

#[test]
fn json_recipe_executes_all_real_builders() {
    with_fake_podman(|| {
        let workspace = tempdir().unwrap();
        let (oci_server, image_ref, pinned_digest) = spawn_test_oci_registry();
        let source_tar = {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut tar = tar::Builder::new(encoder);
            let body = b"hello from json runtime\n";
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
        let source_hash = format!("sha256:{:x}", Sha256::digest(&source_tar));
        let recipe = make_full_recipe(&url, &source_hash, &image_ref, &pinned_digest);
        let recipe_path = workspace.path().join("recipe.json");
        write_recipe(&recipe_path, &recipe);

        let build = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
        handle.join().unwrap();

        let layout = StoreLayout::discover(&workspace.path().join(".mbuild")).unwrap();
        let published = load_build_handle(&layout, build.build_key)
            .unwrap()
            .expect("expected final Build to exist in store");

        assert_eq!(
            published.build.meta["kind"],
            Value::String("container-image".to_string())
        );
        assert_eq!(
            published.build.meta["mode"],
            Value::String("bootstrap".to_string())
        );

        for name in ["source", "script", "base-image", "binary", "final-image"] {
            assert!(
                workspace
                    .path()
                    .join(".mbuild")
                    .join("meta-refs")
                    .join(format!("{name}.json"))
                    .exists()
            );
            assert!(
                workspace
                    .path()
                    .join(".mbuild")
                    .join("object-refs")
                    .join(name)
                    .exists()
            );
        }

        let builds_dir = workspace.path().join(".mbuild").join("builds");
        let objects_dir = workspace.path().join(".mbuild").join("objects");
        assert_eq!(fs::read_dir(&builds_dir).unwrap().count(), 5);
        assert_eq!(fs::read_dir(&objects_dir).unwrap().count(), 5);
        drop(oci_server);
    });
}

#[test]
fn repeated_build_keys_are_built_once_but_published_under_all_names() {
    with_fake_podman(|| {
        let workspace = tempdir().unwrap();
        let (oci_server, image_ref, pinned_digest) = spawn_test_oci_registry();
        let source_tar = {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut tar = tar::Builder::new(encoder);
            let body = b"hello from duplicate test\n";
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
        let source_hash = format!("sha256:{:x}", Sha256::digest(&source_tar));
        let recipe = recipe_node(
            "final-image",
            "Image",
            json!({ "mode": "bootstrap" }),
            json!({
                "base": null,
                "inputs": [
                    binary_recipe("binary-a", &url, &source_hash, &image_ref, &pinned_digest),
                    binary_recipe("binary-b", &url, &source_hash, &image_ref, &pinned_digest)
                ]
            }),
        );
        let recipe_path = workspace.path().join("dedup.json");
        write_recipe(&recipe_path, &recipe);

        let build = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
        handle.join().unwrap();

        let layout = StoreLayout::discover(&workspace.path().join(".mbuild")).unwrap();
        assert!(
            load_build_handle(&layout, build.build_key)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fs::read_dir(workspace.path().join(".mbuild").join("builds"))
                .unwrap()
                .count(),
            5
        );
        assert!(
            workspace
                .path()
                .join(".mbuild")
                .join("meta-refs")
                .join("binary-a.json")
                .exists()
        );
        assert!(
            workspace
                .path()
                .join(".mbuild")
                .join("meta-refs")
                .join("binary-b.json")
                .exists()
        );
        assert!(build_ref_path(workspace.path(), build.build_key).exists());
        drop(oci_server);
    });
}

#[test]
fn independent_fetch_sources_run_in_parallel() {
    with_fake_podman(|| {
        let workspace = tempdir().unwrap();
        let (oci_server, image_ref, pinned_digest) = spawn_test_oci_registry();
        let source_tar = {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut tar = tar::Builder::new(encoder);
            let body = b"hello from parallel test\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("pkg/README.txt").unwrap();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &body[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap()
        };
        let (base_url, handle) = spawn_barrier_http_server(
            source_tar.clone(),
            "application/gzip",
            2,
            Duration::from_secs(2),
        )
        .unwrap();
        let source_hash = format!("sha256:{:x}", Sha256::digest(&source_tar));
        let recipe = binary_with_two_sources_recipe(
            &format!("{base_url}?a=1"),
            &format!("{base_url}?a=2"),
            &source_hash,
            &image_ref,
            &pinned_digest,
        );
        let recipe_path = workspace.path().join("parallel.json");
        write_recipe(&recipe_path, &recipe);

        let build = run_recipe_json_in_workspace_with_options(
            workspace.path(),
            &recipe_path,
            BuildRunOptions {
                emit_progress: false,
                jobs: 4,
            },
        )
        .unwrap();
        handle.join().unwrap();

        let layout = StoreLayout::discover(&workspace.path().join(".mbuild")).unwrap();
        let published = load_build_handle(&layout, build.build_key)
            .unwrap()
            .expect("expected binary Build to exist in store");
        assert_eq!(
            published.build.meta["kind"],
            Value::String("binary-output".to_string())
        );
        drop(oci_server);
    });
}
