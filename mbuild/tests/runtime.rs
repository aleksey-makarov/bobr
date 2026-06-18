mod support;

#[cfg(feature = "integration-tests")]
use bobr_store::fs_tree::{FsTreeEntry, FsTreeManifest};
use bobr_store::{Store, load_build_handle, load_object_record, load_publication};
use mbuild::recipe_runtime::run_recipe_json_in_workspace;
use mbuild_core::BuildKey;
use serde_json::{Value, json};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
#[cfg(feature = "integration-tests")]
use std::process::Command;
#[cfg(feature = "integration-tests")]
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use std::time::Instant;
use support::{
    base_image_recipe, build_ref_count, group_recipe, recipe_node, remove_build_ref,
    remove_object_record, source_recipe, spawn_test_oci_registry, store_root, tree_file_recipe,
    write_recipe, write_recipe_with_options,
};
#[cfg(feature = "integration-tests")]
use support::{tree_directory_recipe, tree_symlink_recipe};
use tempfile::tempdir;

fn source_build_key(object_hash: fsobj_hash::ObjectHash) -> BuildKey {
    BuildKey::from_object_hash(object_hash)
}

#[cfg(feature = "integration-tests")]
fn run_recipe_json_via_cli(recipe_path: &Path) -> bobr_store::RealizedObject {
    let output = Command::new(env!("CARGO_BIN_EXE_mbuild"))
        .arg("--quiet")
        .arg(recipe_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mbuild failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn registered_builders_include_current_tags_only() {
    let mut registry = mbuild_builder::BuilderRegistry::new();
    mbuild_builder::register_in_tree_builders(&mut registry).unwrap();
    bobr_sandbox::register_builders(&mut registry).unwrap();
    let tags = registry.supported_tags();
    for tag in [
        "Group",
        "FsTreeImport",
        "Tree",
        "TreeSubset",
        "TreeMerge",
        "ErofsRootfs",
        "Initramfs",
        "Sandbox",
        "OciExtract",
    ] {
        assert!(tags.contains(&tag), "missing registered builder tag {tag}");
    }
    for tag in [
        "Text",
        "Binary",
        "Container",
        "Rootfs",
        "Ext4Rootfs",
        "Image",
    ] {
        assert!(
            !tags.contains(&tag),
            "unsupported builder tag {tag} is still registered"
        );
    }
}

#[test]
fn group_root_builds_independent_inputs() {
    let workspace = tempdir().unwrap();
    let recipe = recipe_node(
        "all-targets",
        "Group",
        json!({}),
        json!({
            "first": tree_file_recipe("first-target", "first.txt", "first\n", false),
            "second": tree_file_recipe("second-target", "second.txt", "second\n", false),
        }),
    );
    let recipe_path = workspace.path().join("group.json");
    write_recipe(&recipe_path, &recipe);

    let realized = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let root_publication = load_publication(&layout, "all-targets")
        .unwrap()
        .expect("expected root publication");
    assert_eq!(
        root_publication.object_record.object_hash,
        realized.object_hash
    );
    assert_eq!(fs::read(&root_publication.object_path).unwrap(), b"");

    for name in ["all-targets", "first-target", "second-target"] {
        assert!(
            load_publication(&layout, name).unwrap().is_some(),
            "missing publication {name}"
        );
    }
}

fn remove_publication_refs(workspace_root: &Path, name: &str) {
    let store = store_root(workspace_root);
    let refs = [
        store
            .join("object-record-refs")
            .join(format!("{name}.json")),
        store.join("object-refs").join(name),
    ];
    for path in refs {
        if path.exists() || path.is_symlink() {
            fs::remove_file(path).unwrap();
        }
    }
}

fn source_recipe_node(name: &str, object_hash: &str, origin_path: &str, unpack: bool) -> Value {
    json!({
        "name": name,
        "tag": "Source",
        "object_hash": object_hash,
        "origin": {
            "tag": "Path",
            "path": origin_path,
            "unpack": unpack
        },
    })
}

#[cfg(feature = "integration-tests")]
fn ownership_runtime_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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

fn source_tree_hash(source_tar: &[u8]) -> String {
    let temp = tempdir().unwrap();
    let staged = temp.path().join("staged");
    fs::create_dir_all(&staged).unwrap();
    let decoder = flate2::read::GzDecoder::new(Cursor::new(source_tar));
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&staged).unwrap();

    let mut entries = fs::read_dir(&staged)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    if entries.len() == 1 {
        let only = entries.remove(0);
        if only.file_type().unwrap().is_dir() {
            let only_path = only.path();
            for child in fs::read_dir(&only_path).unwrap() {
                let child = child.unwrap();
                fs::rename(child.path(), staged.join(child.file_name())).unwrap();
            }
            fs::remove_dir(&only_path).unwrap();
        }
    }

    fsobj_hash::hash_path(&staged).unwrap().to_string()
}

fn make_full_recipe(
    url: &str,
    source_hash: &str,
    image: &str,
    digest: &str,
    image_object_hash: &str,
) -> Value {
    group_recipe(
        "final-group",
        vec![
            named_source_recipe("source", url, source_hash),
            base_image_recipe(image, digest, image_object_hash),
        ],
    )
}

fn named_source_recipe(name: &str, url: &str, source_hash: &str) -> Value {
    json!({
        "name": name,
        "tag": "Source",
        "object_hash": source_hash,
        "origin": {
            "tag": "Http",
            "url": url,
            "unpack": true
        },
    })
}

fn group_with_two_sources_recipe(url_a: &str, url_b: &str, source_hash: &str) -> Value {
    group_recipe(
        "final-group",
        vec![
            json!({
                "name": "source-a",
                "tag": "Source",
                "object_hash": source_hash,
                "origin": {
                    "tag": "Http",
                    "url": url_a,
                    "unpack": true
                },
            }),
            json!({
                "name": "source-b",
                "tag": "Source",
                "object_hash": source_hash,
                "origin": {
                    "tag": "Http",
                    "url": url_b,
                    "unpack": true
                },
            }),
        ],
    )
}

#[test]
fn json_recipe_executes_source_and_group_graph() {
    let workspace = tempdir().unwrap();
    let (oci_server, image_ref, pinned_digest, image_object_hash) = spawn_test_oci_registry();
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
    let source_hash = source_tree_hash(&source_tar);
    let recipe = make_full_recipe(
        &url,
        &source_hash,
        &image_ref,
        &pinned_digest,
        &image_object_hash,
    );
    let recipe_path = workspace.path().join("recipe.json");
    write_recipe(&recipe_path, &recipe);

    let build = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    handle.join().unwrap();

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let published = load_build_handle(&layout, build.build_key.expect("builder root"))
        .unwrap()
        .expect("expected final Build to exist in store");

    assert!(published.object_path.is_file());

    for name in ["source", "base-image", "final-group"] {
        assert!(
            load_publication(&layout, name).unwrap().is_some(),
            "missing publication {name}"
        );
    }

    let objects_dir = store_root(workspace.path()).join("objects");
    assert_eq!(build_ref_count(workspace.path()), 3);
    assert_eq!(fs::read_dir(&objects_dir).unwrap().count(), 3);
    drop(oci_server);
}

#[test]
fn repeated_build_keys_are_built_once_with_one_publish_name() {
    let workspace = tempdir().unwrap();
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
    let source_hash = source_tree_hash(&source_tar);
    let recipe = recipe_node(
        "final-group",
        "Group",
        json!({}),
        json!({
            "in000": named_source_recipe("source-a", &url, &source_hash),
            "in001": named_source_recipe("source-b", &url, &source_hash)
        }),
    );
    let recipe_path = workspace.path().join("dedup.json");
    write_recipe(&recipe_path, &recipe);

    let build = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    handle.join().unwrap();

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    assert!(
        load_build_handle(&layout, build.build_key.expect("builder root"))
            .unwrap()
            .is_some()
    );
    assert_eq!(build_ref_count(workspace.path()), 2);
    assert!(load_publication(&layout, "source-a").unwrap().is_some());
    assert!(load_publication(&layout, "source-b").unwrap().is_none());
}

#[test]
fn second_run_reuses_root_and_republishes_dependency_refs() {
    let workspace = tempdir().unwrap();
    let source_tar = {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello from root reuse test\n";
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
    let source_hash = source_tree_hash(&source_tar);
    let recipe = group_recipe(
        "final-group",
        vec![named_source_recipe("source", &url, &source_hash)],
    );
    let recipe_path = workspace.path().join("root-reuse.json");
    write_recipe(&recipe_path, &recipe);

    let first = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    handle.join().unwrap();

    assert!(
        load_build_handle(
            &Store::create(&store_root(workspace.path())).unwrap(),
            first.build_key.expect("builder root"),
        )
        .unwrap()
        .is_some()
    );

    for name in ["source", "final-group"] {
        remove_publication_refs(workspace.path(), name);
    }

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();

    assert_eq!(first.build_key, second.build_key);
    let layout = Store::create(&store_root(workspace.path())).unwrap();
    // Every node now runs through the executor on each run, so reused
    // dependencies republish their refs alongside the root.
    assert!(load_publication(&layout, "final-group").unwrap().is_some());
    assert!(load_publication(&layout, "source").unwrap().is_some());
}

#[test]
fn second_run_reuses_root_without_source_materialization() {
    let workspace = tempdir().unwrap();
    let source_tar = {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello from lazy source materialization test\n";
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
    let source_hash = source_tree_hash(&source_tar);
    let recipe = group_recipe(
        "final-group",
        vec![named_source_recipe("source", &url, &source_hash)],
    );
    let recipe_path = workspace.path().join("root-reuse-no-local.json");
    write_recipe(&recipe_path, &recipe);

    let first = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    handle.join().unwrap();

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();

    assert_eq!(first.build_key, second.build_key);
}

/// Counts per-node workspace dirs (those with a `meta.json`) across all runs.
fn workspace_dir_count(workspace_root: &Path) -> usize {
    let logs = store_root(workspace_root).join("logs");
    let Ok(runs) = fs::read_dir(&logs) else {
        return 0;
    };
    let mut count = 0;
    for run in runs.flatten() {
        let run_path = run.path();
        if !run_path.is_dir() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&run_path) {
            for entry in entries.flatten() {
                if entry.path().join("meta.json").is_file() {
                    count += 1;
                }
            }
        }
    }
    count
}

#[test]
fn second_cached_run_creates_no_new_workspaces() {
    let workspace = tempdir().unwrap();
    let recipe = recipe_node(
        "all-targets",
        "Group",
        json!({}),
        json!({
            "only": tree_file_recipe("only-target", "f.txt", "hi\n", false),
        }),
    );
    let recipe_path = workspace.path().join("cached.json");
    write_recipe(&recipe_path, &recipe);

    let first = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let after_first = workspace_dir_count(workspace.path());
    assert!(
        after_first >= 2,
        "expected per-node workspaces on the first (miss) run, got {after_first}"
    );

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let after_second = workspace_dir_count(workspace.path());

    assert_eq!(first.object_hash, second.object_hash);
    assert_eq!(
        after_second, after_first,
        "a fully cached run must not create new per-node workspaces"
    );
}

#[test]
fn identical_fetch_sources_are_deduped_by_object_hash() {
    let workspace = tempdir().unwrap();
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
        1,
        Duration::from_secs(2),
    )
    .unwrap();
    let source_hash = source_tree_hash(&source_tar);
    let recipe = group_with_two_sources_recipe(
        &format!("{base_url}?a=1"),
        &format!("{base_url}?a=2"),
        &source_hash,
    );
    let recipe_path = workspace.path().join("parallel.json");
    write_recipe_with_options(&recipe_path, &recipe, &json!({ "jobs": 4 }));

    let build = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    handle.join().unwrap();

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let published = load_build_handle(&layout, build.build_key.expect("builder root"))
        .unwrap()
        .expect("expected Group Build to exist in store");
    assert!(published.object_path.is_file());
}

#[test]
fn tree_file_recipe_builds_successfully_via_runtime() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("tree-file.json");
    write_recipe(
        &recipe_path,
        &tree_file_recipe("hello-tree", "hello.txt", "hello tree\n", false),
    );

    let build = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let published = load_build_handle(&layout, build.build_key.expect("builder root"))
        .unwrap()
        .expect("expected Tree Build to exist in store");
    assert!(published.object_path.is_file());
    assert_eq!(
        fs::read_to_string(&published.object_path).unwrap(),
        "hello tree\n"
    );
}

#[test]
#[cfg(feature = "integration-tests")]
fn tree_directory_recipe_builds_successfully_via_runtime() {
    let _guard = ownership_runtime_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("tree-dir.json");
    write_recipe(&recipe_path, &tree_directory_recipe("runtime-tree"));

    let build = run_recipe_json_via_cli(&recipe_path);

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let published = load_build_handle(&layout, build.build_key.expect("builder root"))
        .unwrap()
        .expect("expected Tree Build to exist in store");

    assert!(published.object_path.is_file());
    let publication = load_publication(&layout, "runtime-tree")
        .unwrap()
        .expect("expected publication");
    assert_eq!(
        publication.object_record.object_hash,
        published.build.object_hash
    );
    assert_eq!(publication.object_path, published.object_path);
    let manifest = FsTreeManifest::read_canonical(&published.object_path).unwrap();
    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::directory("", 0, 0, 0o755))
    );
    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::directory("dev", 0, 0, 0o755))
    );
    assert!(
        manifest
            .entries()
            .iter()
            .any(|entry| matches!(entry, FsTreeEntry::File { path, .. } if path == "etc/hostname"))
    );
    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::symlink("bin", 0, 0, "usr/bin"))
    );
}

#[test]
#[cfg(feature = "integration-tests")]
fn tree_symlink_recipe_builds_successfully_via_runtime() {
    let _guard = ownership_runtime_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("recipe.json");
    write_recipe(&recipe_path, &tree_symlink_recipe("runtime-tree-symlink"));

    let build = run_recipe_json_via_cli(&recipe_path);

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let published = load_build_handle(&layout, build.build_key.expect("builder root"))
        .unwrap()
        .expect("expected Tree Build to exist in store");

    assert!(published.object_path.is_file());
    let manifest = FsTreeManifest::read_canonical(&published.object_path).unwrap();
    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::symlink("bin", 0, 0, "usr/bin"))
    );
    assert!(manifest.entries().contains(&FsTreeEntry::symlink(
        "etc/mtab",
        0,
        0,
        "/proc/self/mounts"
    )));
}

#[test]
fn source_path_file_materializes_known_object_with_source_build_handle() {
    let workspace = tempdir().unwrap();
    let source_path = workspace.path().join("payload.txt");
    fs::write(&source_path, b"hello source\n").unwrap();
    let object_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let recipe_path = workspace.path().join("source-file.json");
    write_recipe(
        &recipe_path,
        &source_recipe_node(
            "source-file",
            &object_hash.to_string(),
            &source_path.to_string_lossy(),
            false,
        ),
    );

    let realized = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let build_key = source_build_key(object_hash);
    assert_eq!(realized.build_key, Some(build_key));
    assert_eq!(realized.object_hash, object_hash);
    assert!(object_path_exists(&layout, object_hash));
    let published = load_build_handle(&layout, build_key)
        .unwrap()
        .expect("expected source build handle");
    assert_eq!(published.object_record.object_hash, object_hash);
    let result = load_object_record(&layout, realized.object_hash)
        .unwrap()
        .expect("expected source object record");
    assert_eq!(result.object_hash, object_hash);
    assert_eq!(build_ref_count(workspace.path()), 1);
}

#[test]
fn source_path_tar_materializes_unpacked_tree_with_source_build_handle() {
    let workspace = tempdir().unwrap();
    let tar_path = workspace.path().join("payload.tar");
    {
        let file = fs::File::create(&tar_path).unwrap();
        let mut tar = tar::Builder::new(file);
        let body = b"hello tar source\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/README.txt").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &body[..]).unwrap();
        tar.finish().unwrap();
    }
    let object_hash = fsobj_hash::hash_tar_file(&tar_path).unwrap();
    let recipe_path = workspace.path().join("source-tar.json");
    write_recipe(
        &recipe_path,
        &source_recipe_node(
            "source-tar",
            &object_hash.to_string(),
            &tar_path.to_string_lossy(),
            true,
        ),
    );

    let realized = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    let publication = load_publication(&layout, "source-tar")
        .unwrap()
        .expect("expected publication");
    let object_path = publication.object_path;
    let build_key = source_build_key(object_hash);
    assert_eq!(realized.build_key, Some(build_key));
    assert_eq!(realized.object_hash, object_hash);
    assert_eq!(publication.object_record.object_hash, object_hash);
    let published = load_build_handle(&layout, build_key)
        .unwrap()
        .expect("expected source build handle");
    assert_eq!(published.object_record.object_hash, object_hash);
    assert!(object_path.is_dir());
    assert_eq!(
        fs::read_to_string(object_path.join("pkg/README.txt")).unwrap(),
        "hello tar source\n"
    );
    assert!(
        load_object_record(&layout, realized.object_hash)
            .unwrap()
            .is_some()
    );
}

#[test]
fn source_http_mismatch_imports_actual_object_without_canonical_record() {
    let workspace = tempdir().unwrap();
    let source_tar = {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello mismatch\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/README.txt").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &body[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    };
    let actual_hash = source_tree_hash(&source_tar);
    let wrong_hash = "1111111111111111111111111111111111111111111111111111111111111111";
    let (url, handle) = match spawn_http_server(source_tar, "application/gzip") {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("failed to start test HTTP server: {error}"),
    };
    let recipe_path = workspace.path().join("source-http-mismatch.json");
    write_recipe(&recipe_path, &source_recipe(&url, wrong_hash));

    let error = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap_err();
    handle.join().unwrap();

    let message = error.to_string();
    assert!(message.contains(&actual_hash), "{message}");

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    assert!(object_path_exists(&layout, actual_hash.parse().unwrap()));
    assert!(
        load_object_record(&layout, wrong_hash.parse().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn source_oci_registry_mismatch_imports_actual_object_without_canonical_record() {
    let workspace = tempdir().unwrap();
    let (_oci_server, image_ref, pinned_digest, actual_hash) = spawn_test_oci_registry();
    let wrong_hash = "1111111111111111111111111111111111111111111111111111111111111111";
    let recipe_path = workspace.path().join("source-oci-registry-mismatch.json");
    write_recipe(
        &recipe_path,
        &base_image_recipe(&image_ref, &pinned_digest, wrong_hash),
    );

    let error = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap_err();
    let message = error.to_string();
    assert!(message.contains(&actual_hash), "{message}");

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    assert!(object_path_exists(&layout, actual_hash.parse().unwrap()));
    assert!(
        load_object_record(&layout, wrong_hash.parse().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn source_http_mismatch_second_run_reuses_stored_object_without_second_download() {
    let workspace = tempdir().unwrap();
    let source_tar = {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello mismatch retry\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/README.txt").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &body[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    };
    let actual_hash = source_tree_hash(&source_tar);
    let (url, handle) = match spawn_http_server(source_tar, "application/gzip") {
        Ok(server) => server,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("failed to start test HTTP server: {error}"),
    };
    let recipe_path = workspace.path().join("source-http-mismatch-retry.json");
    write_recipe(
        &recipe_path,
        &source_recipe(
            &url,
            "1111111111111111111111111111111111111111111111111111111111111111",
        ),
    );
    let first_error = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap_err();
    handle.join().unwrap();
    assert!(
        first_error.to_string().contains(&actual_hash),
        "{first_error}"
    );

    write_recipe(&recipe_path, &source_recipe(&url, &actual_hash));
    let realized = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    assert_eq!(realized.object_hash.to_string(), actual_hash);
}

#[test]
fn source_oci_registry_mismatch_second_run_reuses_stored_object_without_second_fetch() {
    let workspace = tempdir().unwrap();
    let (oci_server, image_ref, pinned_digest, actual_hash) = spawn_test_oci_registry();
    let recipe_path = workspace
        .path()
        .join("source-oci-registry-mismatch-retry.json");
    write_recipe(
        &recipe_path,
        &base_image_recipe(
            &image_ref,
            &pinned_digest,
            "1111111111111111111111111111111111111111111111111111111111111111",
        ),
    );

    let first_error = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap_err();
    assert!(
        first_error.to_string().contains(&actual_hash),
        "{first_error}"
    );
    drop(oci_server);

    write_recipe(
        &recipe_path,
        &base_image_recipe(&image_ref, &pinned_digest, &actual_hash),
    );
    let realized = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    assert_eq!(realized.object_hash.to_string(), actual_hash);
}

#[test]
fn source_path_mismatch_imports_actual_object_for_follow_up_reuse() {
    let workspace = tempdir().unwrap();
    let source_path = workspace.path().join("payload.txt");
    fs::write(&source_path, b"hello mismatch source\n").unwrap();
    let actual_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let wrong_hash = "1111111111111111111111111111111111111111111111111111111111111111";
    let recipe_path = workspace.path().join("source-path-mismatch.json");
    write_recipe(
        &recipe_path,
        &source_recipe_node(
            "source-file",
            wrong_hash,
            &source_path.to_string_lossy(),
            false,
        ),
    );

    let error = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap_err();
    assert!(
        error.to_string().contains(&actual_hash.to_string()),
        "{error}"
    );

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    assert!(object_path_exists(&layout, actual_hash));
    assert!(
        load_object_record(&layout, wrong_hash.parse().unwrap())
            .unwrap()
            .is_none()
    );

    write_recipe(
        &recipe_path,
        &json!({
            "name": "source-file",
            "tag": "Source",
            "object_hash": actual_hash.to_string(),
        }),
    );
    let realized = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    assert_eq!(realized.object_hash, actual_hash);
}

#[test]
fn source_without_origin_reuses_existing_canonical_object() {
    let workspace = tempdir().unwrap();
    let source_path = workspace.path().join("payload.txt");
    fs::write(&source_path, b"hello source\n").unwrap();
    let object_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let materialized_recipe_path = workspace.path().join("source-materialized.json");
    write_recipe(
        &materialized_recipe_path,
        &source_recipe_node(
            "source-file",
            &object_hash.to_string(),
            &source_path.to_string_lossy(),
            false,
        ),
    );
    let first = run_recipe_json_in_workspace(workspace.path(), &materialized_recipe_path).unwrap();

    let cutoff_recipe_path = workspace.path().join("source-cutoff.json");
    write_recipe(
        &cutoff_recipe_path,
        &json!({
            "name": "source-file",
            "tag": "Source",
            "object_hash": object_hash.to_string(),
        }),
    );

    let second = run_recipe_json_in_workspace(workspace.path(), &cutoff_recipe_path).unwrap();
    assert_eq!(first.object_hash, second.object_hash);
    assert_eq!(second.build_key, Some(source_build_key(object_hash)));
}

#[test]
fn source_without_origin_reuses_existing_oci_layout_object() {
    let workspace = tempdir().unwrap();
    let (_oci_server, image_ref, pinned_digest, object_hash) = spawn_test_oci_registry();
    let materialized_recipe_path = workspace.path().join("source-oci-registry.json");
    write_recipe(
        &materialized_recipe_path,
        &base_image_recipe(&image_ref, &pinned_digest, &object_hash),
    );
    let first = run_recipe_json_in_workspace(workspace.path(), &materialized_recipe_path).unwrap();

    let cutoff_recipe_path = workspace.path().join("source-oci-registry-cutoff.json");
    write_recipe(
        &cutoff_recipe_path,
        &json!({
            "name": "base-image",
            "tag": "Source",
            "object_hash": object_hash,
        }),
    );

    let second = run_recipe_json_in_workspace(workspace.path(), &cutoff_recipe_path).unwrap();
    assert_eq!(first.object_hash, second.object_hash);
    assert_eq!(
        second.build_key,
        Some(source_build_key(object_hash.parse().unwrap()))
    );
}

#[test]
fn source_without_origin_republishes_existing_object() {
    let workspace = tempdir().unwrap();
    let source_path = workspace.path().join("payload.txt");
    fs::write(&source_path, b"hello source\n").unwrap();
    let object_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let materialized_recipe_path = workspace.path().join("source-materialized.json");
    write_recipe(
        &materialized_recipe_path,
        &source_recipe_node(
            "source-file",
            &object_hash.to_string(),
            &source_path.to_string_lossy(),
            false,
        ),
    );
    let first = run_recipe_json_in_workspace(workspace.path(), &materialized_recipe_path).unwrap();

    let layout = Store::create(&store_root(workspace.path())).unwrap();
    remove_build_ref(
        workspace.path(),
        first.build_key.expect("expected source build key"),
    );
    remove_object_record(workspace.path(), first.object_hash);

    let recipe_path = workspace.path().join("source-cutoff-missing-record.json");
    write_recipe(
        &recipe_path,
        &json!({
            "name": "source-cutoff",
            "tag": "Source",
            "object_hash": object_hash.to_string(),
        }),
    );

    let second = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap();
    let restored = load_object_record(&layout, second.object_hash)
        .unwrap()
        .expect("expected restored object record");
    assert_eq!(restored.object_hash, object_hash);
}

#[test]
fn source_without_origin_requires_existing_object_or_record() {
    let workspace = tempdir().unwrap();
    let recipe_path = workspace.path().join("source-cutoff-missing-record.json");
    write_recipe(
        &recipe_path,
        &json!({
            "name": "source-cutoff",
            "tag": "Source",
            "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
        }),
    );

    let error = run_recipe_json_in_workspace(workspace.path(), &recipe_path).unwrap_err();
    assert!(
        error.to_string().contains("has no origin and object"),
        "{error}"
    );
}

fn object_path_exists(layout: &Store, object_hash: fsobj_hash::ObjectHash) -> bool {
    layout.object_path(object_hash).exists()
}
