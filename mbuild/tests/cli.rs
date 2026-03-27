use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

const STORE_RECIPE_TEMPLATE: &str = include_str!("assets/store_recipe_full.ncl");

fn nickel_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

fn write_recipe(recipe_path: &std::path::Path, recipe_source: &str) {
    if let Some(parent) = recipe_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(recipe_path, recipe_source).unwrap();
}

fn text_recipe(name: &str, kind: &str, source: &str) -> String {
    format!(
        "store.text {} {{\n  kind = {},\n  source = {},\n}}\n",
        nickel_string(name),
        nickel_string(kind),
        nickel_string(source),
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

fn wrong_kind_binary_recipe(name: &str) -> String {
    format!(
        "store.bind (store.text \"not-image\" {{\n  kind = \"plain-text\",\n  source = \"not an image\",\n}}) (fun image =>\nstore.bind (store.text \"script\" {{\n  kind = \"build-script\",\n  source = \"#!/bin/sh\\nexit 0\\n\",\n}}) (fun script =>\nstore.binary {} {{\n  kind = \"binary-output\",\n  optimize = \"size\",\n}} image script []))\n",
        nickel_string(name),
    )
}

fn unknown_builder_recipe() -> &'static str {
    "'RunBuilder {\n  name = \"unknown\",\n  tag = \"UnknownBuilder\",\n  config = {},\n  inputs = {},\n}\n"
}

fn install_fake_podman(dir: &std::path::Path, inspect_json: &str) {
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

#[test]
fn cli_uses_default_dot_mbuild_recipe_ncl() {
    let workspace = tempdir().unwrap();
    write_recipe(
        &workspace.path().join(".mbuild").join("recipe.ncl"),
        &text_recipe("default-recipe", "plain-text", "hello default"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("build_key: "));
    assert!(stdout.contains("object_hash: "));
    assert!(!stdout.contains("build_key: sha256:"));
    assert!(!stdout.contains("object_hash: sha256:"));
    assert!(stdout.contains("object_path:"));
    assert!(stderr.contains("[start] Text default-recipe"), "{stderr}");
    assert!(stderr.contains("[done] Text default-recipe"), "{stderr}");
}

#[test]
fn cli_accepts_explicit_recipe_path() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("custom.ncl");
    write_recipe(
        &recipe_path,
        &text_recipe("custom-recipe", "plain-text", "hello custom"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("build_key: "));
    assert!(stdout.contains("object_hash: "));
    assert!(!stdout.contains("build_key: sha256:"));
    assert!(!stdout.contains("object_hash: sha256:"));
    assert!(stderr.contains("[start] Text custom-recipe"), "{stderr}");
}

#[test]
fn cli_quiet_suppresses_live_progress() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("quiet.ncl");
    write_recipe(
        &recipe_path,
        &text_recipe("quiet-recipe", "plain-text", "hello quiet"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg("--quiet")
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn cli_prints_unit_for_return_null() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("unit.ncl");
    write_recipe(&recipe_path, "'Return null\n");

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "()\n");
}

#[test]
fn cli_sequence_ignores_results() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("sequence-discard.ncl");
    write_recipe(
        &recipe_path,
        "store.sequence_ [store.return 1, store.return 2]\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "()\n");
}

#[test]
fn cli_for_each_ignores_results() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("for-each.ncl");
    write_recipe(
        &recipe_path,
        "store.for_each [1, 2] (fun x => store.return x)\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "()\n");
}

#[test]
fn cli_reports_recipe_parse_errors_with_nickel_diagnostics() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("bad-syntax.ncl");
    write_recipe(&recipe_path, "let = 1\n");

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error: unexpected token"), "{stderr}");
    assert!(stderr.contains("bad-syntax.ncl:1:5"), "{stderr}");
    assert!(!stderr.contains("ParseErrors("), "{stderr}");
    assert!(!stderr.contains("error[invalid-input]:"), "{stderr}");
}

#[test]
fn cli_executes_container_image_recipe_with_fake_podman() {
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
        .arg(&recipe_path)
        .env("PATH", path_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);

    let refs_dir = workspace.path().join(".mbuild").join("object-refs");
    let published_path = fs::read_link(refs_dir.join("bootstrap-image")).unwrap();
    assert!(published_path.to_string_lossy().contains("../objects/"));
    assert!(
        !published_path
            .to_string_lossy()
            .contains("../objects/sha256:")
    );
}

#[test]
fn cli_rejects_container_image_recipe_when_digest_does_not_match() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("container-image-bad.ncl");
    write_recipe(
        &recipe_path,
        &container_image_recipe(
            "bootstrap-image",
            "docker.io/library/buildpack-deps:bookworm",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ),
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
        .arg(&recipe_path)
        .env("PATH", path_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]:"), "{stderr}");
    assert!(
        stderr.contains("does not match required digest"),
        "{stderr}"
    );
}

#[test]
fn cli_executes_binary_recipe_with_fake_podman_and_nested_inputs() {
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
    let source_hash = format!("sha256:{:x}", Sha256::digest(&source_tar));

    let recipe_path = workspace.path().join("binary.ncl");
    write_recipe(
        &recipe_path,
        &binary_recipe(
            "zstd-bin",
            "docker.io/library/buildpack-deps:bookworm",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &url,
            &source_hash,
        ),
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
        .arg(&recipe_path)
        .env("PATH", path_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();
    handle.join().unwrap();

    assert!(output.status.success(), "{:?}", output);

    let object_path = workspace
        .path()
        .join(".mbuild")
        .join("object-refs")
        .join("zstd-bin");
    let resolved = workspace
        .path()
        .join(".mbuild")
        .join("object-refs")
        .join(fs::read_link(&object_path).unwrap());
    let object_dir = fs::canonicalize(resolved).unwrap();
    assert_eq!(
        fs::read_to_string(object_dir.join("copied").join("README.txt")).unwrap(),
        "hello binary\n"
    );
    assert_eq!(
        fs::read_to_string(object_dir.join("image-ref.txt")).unwrap(),
        "docker.io/library/buildpack-deps@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
    );
}

#[test]
fn cli_executes_image_recipe_with_fake_podman_and_nested_binary() {
    let workspace = tempdir().unwrap();
    let source_tar = {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello image\n";
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

    let recipe_path = workspace.path().join("image.ncl");
    write_recipe(
        &recipe_path,
        &STORE_RECIPE_TEMPLATE
            .replace("__URL__", &url)
            .replace("__SOURCE_HASH__", &source_hash),
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
        .arg(&recipe_path)
        .env("PATH", path_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();
    handle.join().unwrap();

    assert!(output.status.success(), "{:?}", output);

    let ref_path = workspace
        .path()
        .join(".mbuild")
        .join("object-refs")
        .join("final-image");
    let descriptor_path = fs::canonicalize(ref_path).unwrap();
    let descriptor: serde_json::Value =
        serde_json::from_slice(&fs::read(&descriptor_path).unwrap()).unwrap();
    assert_eq!(
        descriptor["schema"],
        serde_json::Value::String("mbuild-container-image-object-v1".to_string())
    );
    assert_eq!(
        descriptor["storage"],
        serde_json::Value::String("external-podman".to_string())
    );
    let image_ref = descriptor["image_ref"].as_str().unwrap();
    assert!(
        image_ref.starts_with("localhost/mbuild-image:bootstrap-"),
        "{image_ref}"
    );
    assert_eq!(
        descriptor["image_digest"],
        serde_json::Value::String(
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_string(),
        )
    );
}

#[test]
fn cli_rejects_unknown_builder_recipe() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("unknown.ncl");
    write_recipe(&recipe_path, unknown_builder_recipe());

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]:"), "{stderr}");
    assert!(
        stderr.contains("unknown builder tag 'UnknownBuilder'"),
        "{stderr}"
    );
}

#[test]
fn cli_rejects_binary_recipe_with_wrong_input_kind() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("binary-wrong-kind.ncl");
    write_recipe(&recipe_path, &wrong_kind_binary_recipe("wrong-kind"));

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]:"), "{stderr}");
    assert!(
        stderr.contains("input slot 'image' rejects kind 'plain-text'"),
        "{stderr}"
    );
}

#[test]
fn cli_rejects_non_store_toplevel_value() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("not-store.ncl");
    write_recipe(&recipe_path, "42\n");

    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg(&recipe_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]:"), "{stderr}");
    assert!(
        stderr.contains("expected STORE action enum variant"),
        "{stderr}"
    );
}
