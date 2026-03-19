use mbuild::store_interpreter::{StoreOutcome, run_store_recipe_in_workspace};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

const STORE_RECIPE_TEMPLATE: &str = include_str!("assets/store_recipe_full.ncl");

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn nickel_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

fn install_fake_podman(dir: &Path, inspect_json: &str) {
    let script_path = dir.join("podman");
    let script = include_str!("assets/fake_podman_full.sh")
        .replace("__INSPECT_JSON__", inspect_json)
        .replace(
            "__GENERATED_DIGEST__",
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        );
    fs::write(&script_path, script).unwrap();
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
    }
}

fn with_fake_podman<T>(inspect_json: &str, f: impl FnOnce() -> T) -> T {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn write_recipe(recipe_path: &Path, recipe_source: &str) {
    if let Some(parent) = recipe_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(recipe_path, recipe_source).unwrap();
}

fn build_path(root: &Path, build_key: impl ToString) -> PathBuf {
    root.join(".mbuild")
        .join("builds")
        .join(format!("{}.json", build_key.to_string()))
}

fn expect_build(outcome: StoreOutcome) -> mbuild_core::PublishedBuild {
    match outcome {
        StoreOutcome::Build(published) => published,
        StoreOutcome::Unit => panic!("expected STORE result to be Build"),
    }
}

fn text_recipe(name: &str, kind: &str, source: &str) -> String {
    format!(
        "store.text {} {{\n  kind = {},\n  source = {},\n}}\n",
        nickel_string(name),
        nickel_string(kind),
        nickel_string(source),
    )
}

fn fetch_recipe(name: &str, url: &str, hash: &str, unpack: bool) -> String {
    format!(
        "store.fetch {} {{\n  url = {},\n  hash = {},\n  unpack = {},\n}}\n",
        nickel_string(name),
        nickel_string(url),
        nickel_string(hash),
        if unpack { "true" } else { "false" },
    )
}

fn container_image_recipe(name: &str, image: &str, digest: &str) -> String {
    format!(
        "store.container_image {} {{\n  image = {},\n  digest = {},\n}}\n",
        nickel_string(name),
        nickel_string(image),
        nickel_string(digest),
    )
}

fn binary_recipe(
    name: &str,
    image: &str,
    digest: &str,
    source_url: &str,
    source_hash: &str,
) -> String {
    format!(
        "store.bind (store.fetch \"source\" {{\n  url = {},\n  hash = {},\n  unpack = true,\n}}) (fun source =>\nstore.bind (store.text \"script\" {{\n  kind = \"build-script\",\n  source = \"#!/bin/sh\\nexit 0\\n\",\n}}) (fun script =>\nstore.bind (store.container_image \"base-image\" {{\n  image = {},\n  digest = {},\n}}) (fun image =>\nstore.binary {} {{\n  kind = \"binary-output\",\n  optimize = \"size\",\n}} image script [source])))\n",
        nickel_string(source_url),
        nickel_string(source_hash),
        nickel_string(image),
        nickel_string(digest),
        nickel_string(name),
    )
}

#[test]
fn store_text_recipe_creates_store_entries_and_refs() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe.ncl");
    write_recipe(
        &recipe_path,
        &text_recipe("hello", "build-script", "#!/bin/sh\necho hi\n"),
    );

    let published =
        expect_build(run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap());

    assert!(published.object_path.exists());
    assert_eq!(
        fs::read_to_string(&published.object_path).unwrap(),
        "#!/bin/sh\necho hi\n"
    );
    #[cfg(unix)]
    {
        let mode = fs::metadata(&published.object_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    let build_file = build_path(workspace.path(), published.record.build_key);
    assert!(build_file.exists());
    let build_json: Value = serde_json::from_slice(&fs::read(&build_file).unwrap()).unwrap();
    assert_eq!(
        build_json["build_key"],
        Value::String(published.record.build_key.to_string())
    );
    assert_eq!(
        build_json["kind"],
        Value::String("build-script".to_string())
    );
    assert_eq!(
        build_json["producer"]["builder"],
        Value::String("text".to_string())
    );
    assert_eq!(
        build_json["object_hash"],
        Value::String(published.record.object_hash.to_string())
    );
    assert_eq!(build_json["input_build_keys"], Value::Array(vec![]));

    assert_eq!(
        fs::read_link(
            workspace
                .path()
                .join(".mbuild")
                .join("meta-refs")
                .join("hello.json")
        )
        .unwrap(),
        PathBuf::from(format!("../builds/{}.json", published.record.build_key))
    );
    assert_eq!(
        fs::read_link(
            workspace
                .path()
                .join(".mbuild")
                .join("object-refs")
                .join("hello")
        )
        .unwrap(),
        PathBuf::from(format!("../objects/{}", published.record.object_hash))
    );
}

#[test]
fn repeated_store_text_recipe_reuses_same_build_record_and_object() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe.ncl");
    write_recipe(
        &recipe_path,
        &text_recipe("cached", "plain-text", "hello cache"),
    );

    let first =
        expect_build(run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap());
    let second =
        expect_build(run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap());

    assert_eq!(first.record.build_key, second.record.build_key);
    assert_eq!(first.record.object_hash, second.record.object_hash);
    assert_eq!(
        fs::read_dir(workspace.path().join(".mbuild").join("builds"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        fs::read_dir(workspace.path().join(".mbuild").join("objects"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn store_recipe_executes_fetch_recipe_end_to_end() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("fetch-recipe.ncl");
    let body = b"fetched payload\n".to_vec();
    let hash = format!("sha256:{:x}", Sha256::digest(&body));
    let (url, handle) = match spawn_http_server(body.clone(), "application/octet-stream") {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("failed to start test HTTP server: {error}"),
    };
    write_recipe(
        &request_path,
        &fetch_recipe("fetched-file", &url, &hash, false),
    );

    let published =
        expect_build(run_store_recipe_in_workspace(workspace.path(), &request_path).unwrap());
    handle.join().unwrap();

    assert_eq!(published.record.kind, "fetched-file");
    assert_eq!(fs::read(&published.object_path).unwrap(), body);
    assert_eq!(published.record.attrs["source_url"], Value::String(url));
    assert_eq!(published.record.attrs["declared_hash"], Value::String(hash));
    assert_eq!(published.record.attrs["unpack"], Value::Bool(false));
}

#[test]
fn store_recipe_executes_container_image_recipe_and_persists_full_record() {
    with_fake_podman(
        r#"[{"Id":"sha256:imageid","RepoDigests":["docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}]"#,
        || {
            let workspace = tempdir().unwrap();
            let recipe_path = workspace.path().join("container-image.ncl");
            write_recipe(
                &recipe_path,
                &container_image_recipe(
                    "bootstrap-image",
                    "docker.io/library/buildpack-deps:bookworm",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            );

            let published = expect_build(
                run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap(),
            );

            assert_eq!(published.record.kind, "container-image");
            assert_eq!(published.record.producer.builder, "container-image");
            assert_eq!(
                published.record.attrs["image"],
                Value::String("docker.io/library/buildpack-deps:bookworm".to_string())
            );
            assert_eq!(
                published.record.attrs["image_ref"],
                Value::String(
                    "docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                )
            );
            assert_eq!(
                published.record.attrs["image_id"],
                Value::String("sha256:imageid".to_string())
            );
            assert_eq!(
                published.record.attrs["image_digest"],
                Value::String(
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                )
            );

            let descriptor: Value =
                serde_json::from_slice(&fs::read(&published.object_path).unwrap()).unwrap();
            assert_eq!(
                descriptor["schema"],
                Value::String("mbuild-container-image-object-v1".to_string())
            );
            assert_eq!(
                descriptor["storage"],
                Value::String("external-podman".to_string())
            );
            assert_eq!(
                descriptor["image_ref"],
                Value::String(
                    "docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                )
            );
            assert_eq!(
                descriptor["image_digest"],
                Value::String(
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                )
            );

            let build_file = build_path(workspace.path(), published.record.build_key);
            let build_json: Value =
                serde_json::from_slice(&fs::read(&build_file).unwrap()).unwrap();
            assert_eq!(
                build_json["kind"],
                Value::String("container-image".to_string())
            );
            assert_eq!(
                build_json["producer"]["builder"],
                Value::String("container-image".to_string())
            );
            assert_eq!(
                build_json["object_hash"],
                Value::String(published.record.object_hash.to_string())
            );
            assert_eq!(
                build_json["attrs"]["image"],
                Value::String("docker.io/library/buildpack-deps:bookworm".to_string())
            );
            assert_eq!(
                build_json["attrs"]["image_ref"],
                Value::String(
                    "docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                )
            );
            assert_eq!(
                build_json["attrs"]["image_id"],
                Value::String("sha256:imageid".to_string())
            );
            assert_eq!(
                build_json["attrs"]["image_digest"],
                Value::String(
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                )
            );
        },
    );
}

#[test]
fn repeated_nested_binary_recipe_reuses_all_build_records_and_objects() {
    with_fake_podman(
        r#"[{"Id":"sha256:imageid","RepoDigests":["docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}]"#,
        || {
            let workspace = tempdir().unwrap();
            let recipe_path = workspace.path().join("binary-recipe.ncl");
            let source_tar = {
                let encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                let mut tar = tar::Builder::new(encoder);
                let body = b"hello cached binary\n";
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
            write_recipe(
                &recipe_path,
                &binary_recipe(
                    "cached-binary",
                    "docker.io/library/buildpack-deps:bookworm",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &url,
                    &source_hash,
                ),
            );

            let first = expect_build(
                run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap(),
            );
            handle.join().unwrap();

            let objects_dir = workspace.path().join(".mbuild").join("objects");
            let builds_dir = workspace.path().join(".mbuild").join("builds");
            let first_object_count = fs::read_dir(&objects_dir).unwrap().count();
            let first_build_count = fs::read_dir(&builds_dir).unwrap().count();

            let second = expect_build(
                run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap(),
            );

            assert_eq!(first.record.build_key, second.record.build_key);
            assert_eq!(first.record.object_hash, second.record.object_hash);
            assert_eq!(
                fs::read_dir(objects_dir).unwrap().count(),
                first_object_count
            );
            assert_eq!(fs::read_dir(builds_dir).unwrap().count(), first_build_count);
            assert_eq!(first_build_count, 4);
            assert_eq!(first_object_count, 4);

            let binary_build_json: Value = serde_json::from_slice(
                &fs::read(build_path(workspace.path(), first.record.build_key)).unwrap(),
            )
            .unwrap();
            let input_build_keys = binary_build_json["input_build_keys"]
                .as_array()
                .expect("binary build record must encode input build keys");
            assert_eq!(
                binary_build_json["build_key"],
                Value::String(first.record.build_key.to_string())
            );
            assert_eq!(input_build_keys.len(), 3);
            assert!(
                input_build_keys
                    .iter()
                    .all(|value| matches!(value, Value::String(_)))
            );
        },
    );
}

#[test]
fn store_recipe_executes_all_real_builders_via_full_template() {
    with_fake_podman(
        r#"[{"Id":"sha256:imageid","RepoDigests":["docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}]"#,
        || {
            let workspace = tempdir().unwrap();
            let source_tar = {
                let encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                let mut tar = tar::Builder::new(encoder);
                let body = b"hello from store loop\n";
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
            let recipe_source = STORE_RECIPE_TEMPLATE
                .replace("__URL__", &url)
                .replace("__SOURCE_HASH__", &source_hash);
            let recipe_path = workspace.path().join("full.ncl");
            write_recipe(&recipe_path, &recipe_source);

            let published = expect_build(
                run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap(),
            );
            handle.join().unwrap();

            assert_eq!(published.record.kind, "container-image");
            assert_eq!(published.record.producer.builder, "image");
            assert_eq!(
                published.record.attrs["mode"],
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
        },
    );
}
