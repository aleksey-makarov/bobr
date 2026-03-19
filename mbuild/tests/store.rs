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

fn write_store_recipe(workspace: &Path, recipe_source: &str) -> PathBuf {
    let recipe_path = workspace.join("recipe.ncl");
    fs::write(&recipe_path, recipe_source).unwrap();
    recipe_path
}

#[test]
fn store_recipe_executes_all_real_builders() {
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
            let recipe_path = write_store_recipe(workspace.path(), &recipe_source);

            let published =
                match run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap() {
                    StoreOutcome::Build(published) => published,
                    StoreOutcome::Unit => panic!("expected final STORE result to be Build"),
                };
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

            let builds_dir = workspace.path().join(".mbuild").join("builds");
            let objects_dir = workspace.path().join(".mbuild").join("objects");
            assert_eq!(fs::read_dir(&builds_dir).unwrap().count(), 5);
            assert_eq!(fs::read_dir(&objects_dir).unwrap().count(), 5);
        },
    );
}

#[test]
fn store_binding_is_visible_in_imported_modules() {
    let workspace = tempdir().unwrap();
    let recipe_path = write_store_recipe(workspace.path(), "import \"./pkg.ncl\"\n");
    fs::write(
        workspace.path().join("pkg.ncl"),
        "store.text \"hello\" {\n  kind = \"plain-text\",\n  source = \"hi from import\\n\",\n}\n",
    )
    .unwrap();

    let published = match run_store_recipe_in_workspace(workspace.path(), &recipe_path).unwrap() {
        StoreOutcome::Build(published) => published,
        StoreOutcome::Unit => panic!("expected final STORE result to be Build"),
    };

    assert_eq!(published.record.kind, "plain-text");
    assert!(
        workspace
            .path()
            .join(".mbuild")
            .join("meta-refs")
            .join("hello.json")
            .exists()
    );
}
