//! Exercise the registered composer in real, reaped namespace worker processes.
#![allow(missing_docs)]

use bobr_core::CancellationToken;
use bobr_runtime::runtime::{NoRuntimeEvent, Runtime, RuntimeError, RuntimeFunction};
use bobr_runtime::runtime_ns::{NsFunction, NsRuntime, run_worker, worker_invocation_from_env};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown, symlink};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

// Deliberately call only through the registered wire contract: no public
// exposure of the builder's internal plan types is needed for this test.
struct Compose;

impl RuntimeFunction for Compose {
    type Input = Value;
    type Output = Value;
    type Event = NoRuntimeEvent;

    fn name(&self) -> &'static str {
        "plain-tree-compose"
    }

    fn call(&self, _: Value) -> Result<Value, RuntimeError> {
        panic!("this test must dispatch to the registered namespace function")
    }
}

fn assert_owner(path: &Path, uid: u32, gid: u32) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert_eq!((metadata.uid(), metadata.gid()), (uid, gid));
    if metadata.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            assert_owner(&entry.unwrap().path(), uid, gid);
        }
    }
}

fn main() {
    if let Some(invocation) = worker_invocation_from_env().unwrap() {
        let mut functions = bobr_builder::runtime_functions();
        functions.push(NsFunction::new(ForeignOwner));
        functions.push(NsFunction::new(RestoreOwner));
        run_worker(invocation, functions).unwrap();
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("nested/tool"), b"tool").unwrap();
    symlink("nested/tool", source.join("link")).unwrap();
    let token = CancellationToken::new_in(temp.path()).unwrap();
    let runtime = NsRuntime::new().unwrap();
    let owner = fs::metadata(temp.path()).unwrap();
    let output = temp.path().join("output");
    let plan = json!({
        "output_root": output,
        "cancellation_fifo": token.fifo_path(),
        "ops": [{"kind": "copy-tree", "source": source, "dest": "root"}]
    });

    // The direct host user cannot read a 0700 directory owned by namespace uid
    // 123. Use a separate test runtime function solely to prepare that input.
    // It is not part of the production registry.
    let result = runtime
        .run(&ForeignOwner, json!({"source": source}))
        .unwrap();
    assert_eq!(result, json!("prepared"));
    let result = runtime.run(&Compose, plan.clone()).unwrap();
    assert_eq!(result, json!("completed"));
    assert_owner(&output, owner.uid(), owner.gid());
    assert_eq!(
        fs::metadata(output.join("root/nested")).unwrap().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        fs::metadata(output.join("root/nested/tool"))
            .unwrap()
            .mode()
            & 0o7777,
        0o6751
    );
    assert_eq!(fs::read(output.join("root/nested/tool")).unwrap(), b"tool");
    assert_eq!(
        fs::read_link(output.join("root/link")).unwrap(),
        Path::new("nested/tool")
    );
    fs::remove_dir_all(&output).unwrap();
    runtime
        .run(&RestoreOwner, json!({"source": source}))
        .unwrap();

    for index in 0..8192 {
        fs::write(source.join(format!("file-{index:05}")), b"payload").unwrap();
    }
    let worker = thread::spawn(move || runtime.run(&Compose, plan));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !output.join("root/file-00000").exists() {
        assert!(!worker.is_finished(), "worker ended before cancellation");
        assert!(Instant::now() < deadline, "worker did not start copying");
        thread::yield_now();
    }
    token.cancel();
    assert_eq!(worker.join().unwrap().unwrap(), json!("cancelled"));
    assert!(!output.exists(), "cancelled output was not removed");
    drop(token);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    // NsRuntime::run waits for the worker. This standalone test has no other
    // child processes: ECHILD proves neither a live worker nor a zombie remains.
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    println!("namespace composition, foreign ownership, modes, and cancellation: ok");
}

struct ForeignOwner;
struct RestoreOwner;

fn change_owner(input: Value, foreign: bool) -> Result<Value, RuntimeError> {
    let source = Path::new(input["source"].as_str().unwrap());
    let (uid, gid) = if foreign { (123, 456) } else { (0, 0) };
    for path in [
        source.to_path_buf(),
        source.join("nested"),
        source.join("nested/tool"),
        source.join("link"),
    ] {
        lchown(path, Some(uid), Some(gid))?;
    }
    fs::set_permissions(
        source.join("nested"),
        fs::Permissions::from_mode(if foreign { 0o700 } else { 0o755 }),
    )?;
    fs::set_permissions(
        source.join("nested/tool"),
        fs::Permissions::from_mode(0o6751),
    )?;
    Ok(json!("prepared"))
}

macro_rules! owner_function {
    ($name:ident, $wire:literal, $foreign:literal) => {
        impl RuntimeFunction for $name {
            type Input = Value;
            type Output = Value;
            type Event = NoRuntimeEvent;
            fn name(&self) -> &'static str {
                $wire
            }
            fn call(&self, input: Value) -> Result<Value, RuntimeError> {
                change_owner(input, $foreign)
            }
        }
    };
}
owner_function!(ForeignOwner, "test-foreign-owner", true);
owner_function!(RestoreOwner, "test-restore-owner", false);
