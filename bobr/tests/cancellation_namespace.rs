//! Check sandbox cancellation input in real, reaped namespace workers.

use bobr_core::CancellationToken;
use bobr_runtime::runtime::Runtime;
use bobr_runtime::runtime_ns::{NsRuntime, run_worker, worker_invocation_from_env};
use bobr_sandbox::{SandboxFunctionOutput, SandboxInput, SandboxInstallFunction};
use bobr_store::Store;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;

fn main() {
    if let Some(invocation) = worker_invocation_from_env().unwrap() {
        run_worker(invocation, bobr_sandbox::runtime_functions()).unwrap();
        return;
    }

    let runtime = NsRuntime::new().unwrap();
    for plain_object in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let tmp = temp.path().join("tmp");
        let store_root = temp.path().join("store");
        for path in [&rootfs, &tmp, &store_root] {
            fs::create_dir(path).unwrap();
        }
        let store = Store::create(&store_root).unwrap();
        let token = CancellationToken::new_in(temp.path()).unwrap();
        // Keep another writer alive throughout the call: EOF is not the signal.
        let _writer = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(temp.path().join("cancel"))
            .unwrap();
        let input: SandboxInput = serde_json::from_value(json!({
            "rootfs": rootfs,
            "config_dir": temp.path().join("must-not-create-config"),
            "tmp": tmp,
            "fs_tree": store.fs_tree(),
            "launcher_path": temp.path().join("must-not-launch"),
            "extra_inputs": [],
            "steps": [],
            "build_seed_hex": "00".repeat(32),
            "plain_object": plain_object,
            "cancellation": token.receiver().unwrap(),
        }))
        .unwrap();
        token.cancel();

        let output = runtime.run(&SandboxInstallFunction, input).unwrap();
        assert!(matches!(output, SandboxFunctionOutput::Cancelled));
        assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0);
        assert!(!temp.path().join("must-not-create-config").exists());
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    println!("sandbox cancellation through namespace input, both output modes: ok");
}
