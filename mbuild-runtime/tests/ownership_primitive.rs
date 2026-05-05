#![cfg(all(feature = "integration-tests", target_os = "linux"))]

use libcontainer::container::builder::ContainerBuilder;
use libcontainer::oci_spec::runtime::{
    Capabilities, Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxIdMappingBuilder,
    LinuxNamespaceBuilder, LinuxNamespaceType, MountBuilder, ProcessBuilder, RootBuilder, Spec,
    SpecBuilder, UserBuilder,
};
use libcontainer::syscall::syscall::SyscallType;
use libcontainer::workload::{
    Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Gid, Uid, User, chown, getegid, geteuid};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tempfile::tempdir;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Clone, Copy)]
struct HostIdmap {
    euid: u32,
    egid: u32,
    subuid_base: u32,
    subuid_count: u32,
    subgid_base: u32,
    subgid_count: u32,
}

impl HostIdmap {
    fn from_host() -> TestResult<Self> {
        let euid = geteuid().as_raw();
        let egid = getegid().as_raw();
        let user = User::from_uid(Uid::from_raw(euid))?
            .ok_or_else(|| format!("current euid {euid} has no passwd entry"))?;
        let username = user.name;
        let (subuid_base, subuid_count) = first_subid_range(Path::new("/etc/subuid"), &username)?;
        let (subgid_base, subgid_count) = first_subid_range(Path::new("/etc/subgid"), &username)?;

        Ok(Self {
            euid,
            egid,
            subuid_base,
            subuid_count,
            subgid_base,
            subgid_count,
        })
    }
}

#[derive(Debug)]
struct Bundle {
    dir: PathBuf,
    state_dir: PathBuf,
    target_dir: PathBuf,
    error_log: PathBuf,
}

impl Bundle {
    fn new(workspace: &Path, idmap: HostIdmap) -> TestResult<Self> {
        let suffix = Uuid::new_v4().simple().to_string();
        let dir = workspace.join(format!("bundle-{suffix}"));
        let rootfs = dir.join("rootfs");
        let state_dir = workspace.join(format!("state-{suffix}"));
        let target_dir = workspace.join(format!("target-{suffix}"));
        let error_log = rootfs.join("error.json");

        fs::create_dir(&dir)?;
        fs::create_dir(&rootfs)?;
        fs::create_dir(&state_dir)?;
        fs::create_dir(&target_dir)?;
        fs::create_dir(rootfs.join("dev"))?;
        fs::create_dir(rootfs.join("target"))?;
        fs::create_dir(rootfs.join("proc"))?;
        fs::write(&error_log, "")?;

        populate_target_tree(&target_dir)?;
        build_spec(idmap, &target_dir).save(dir.join("config.json"))?;

        Ok(Self {
            dir,
            state_dir,
            target_dir,
            error_log,
        })
    }

    fn cleanup(&self) -> TestResult<()> {
        remove_dir_if_exists(&self.dir)?;
        remove_dir_if_exists(&self.state_dir)?;
        Ok(())
    }
}

#[derive(Clone)]
struct OwnershipProbeExecutor {
    fail_after_report: bool,
}

impl Executor for OwnershipProbeExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        if self.fail_after_report {
            let report = ExecutorErrorReport {
                kind: "forced_failure".to_string(),
                path: "/target".to_string(),
                message: "forced ownership primitive failure".to_string(),
                errno: None,
            };
            write_report(&report).map_err(|error| ExecutorError::Other(error.to_string()))?;
            return Err(ExecutorError::Other(report.message));
        }

        ensure_file("/target/root-file")?;
        ensure_file("/target/user-file")?;
        ensure_directory("/target/user-dir")?;
        ensure_symlink("/target/user-link")?;

        chown(
            "/target/root-file",
            Some(Uid::from_raw(0)),
            Some(Gid::from_raw(0)),
        )
        .map_err(executor_error)?;
        chown(
            "/target/user-file",
            Some(Uid::from_raw(1)),
            Some(Gid::from_raw(1)),
        )
        .map_err(executor_error)?;
        chown(
            "/target/user-dir",
            Some(Uid::from_raw(1)),
            Some(Gid::from_raw(1)),
        )
        .map_err(executor_error)?;
        lchown("/target/user-link", 1, 1).map_err(executor_error)?;

        chmod("/target/root-file", 0o644).map_err(executor_error)?;
        chmod("/target/user-file", 0o600).map_err(executor_error)?;
        chmod("/target/user-dir", 0o700).map_err(executor_error)?;

        std::process::exit(0);
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ExecutorErrorReport {
    kind: String,
    path: String,
    message: String,
    errno: Option<i32>,
}

#[test]
fn proves_libcontainer_ownership_primitive() -> TestResult<()> {
    init_tracing();
    let idmap = HostIdmap::from_host()?;
    let temp = tempdir()?;
    let success = Bundle::new(temp.path(), idmap)?;

    run_probe(
        &success,
        OwnershipProbeExecutor {
            fail_after_report: false,
        },
    )?;
    assert_target_tree(&success.target_dir, idmap)?;
    success.cleanup()?;
    assert!(!success.dir.exists());
    assert!(!success.state_dir.exists());

    let failure = Bundle::new(temp.path(), idmap)?;
    let error = run_probe(
        &failure,
        OwnershipProbeExecutor {
            fail_after_report: true,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("wait status"));

    let report = read_report(&failure.error_log)?;
    assert_eq!(report.kind, "forced_failure");
    assert_eq!(report.path, "/target");
    assert!(
        report
            .message
            .contains("forced ownership primitive failure")
    );
    failure.cleanup()?;
    assert!(!failure.dir.exists());
    assert!(!failure.state_dir.exists());

    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn run_probe(bundle: &Bundle, executor: OwnershipProbeExecutor) -> TestResult<()> {
    let container_id = format!("mbuild-runtime-{}", Uuid::new_v4().simple());
    let mut container = ContainerBuilder::new(container_id, SyscallType::Linux)
        .with_executor(executor)
        .with_root_path(&bundle.state_dir)?
        .as_init(&bundle.dir)
        .with_systemd(false)
        .with_detach(false)
        .build()?;

    let pid = container
        .pid()
        .ok_or_else(|| "libcontainer did not expose init pid".to_string())?;
    let start_result = container.start();
    let wait_status = waitpid(pid, None);
    let delete_result = container.delete(true);

    start_result?;
    delete_result?;
    match wait_status? {
        WaitStatus::Exited(_, 0) => Ok(()),
        status => Err(format!("wait status for ownership probe was {status:?}").into()),
    }
}

fn build_spec(idmap: HostIdmap, target_dir: &Path) -> Spec {
    let uid_mappings = vec![
        LinuxIdMappingBuilder::default()
            .container_id(0_u32)
            .host_id(idmap.euid)
            .size(1_u32)
            .build()
            .unwrap(),
        LinuxIdMappingBuilder::default()
            .container_id(1_u32)
            .host_id(idmap.subuid_base)
            .size(idmap.subuid_count)
            .build()
            .unwrap(),
    ];
    let gid_mappings = vec![
        LinuxIdMappingBuilder::default()
            .container_id(0_u32)
            .host_id(idmap.egid)
            .size(1_u32)
            .build()
            .unwrap(),
        LinuxIdMappingBuilder::default()
            .container_id(1_u32)
            .host_id(idmap.subgid_base)
            .size(idmap.subgid_count)
            .build()
            .unwrap(),
    ];

    let linux = LinuxBuilder::default()
        .namespaces(vec![
            LinuxNamespaceBuilder::default()
                .typ(LinuxNamespaceType::User)
                .build()
                .unwrap(),
            LinuxNamespaceBuilder::default()
                .typ(LinuxNamespaceType::Mount)
                .build()
                .unwrap(),
            LinuxNamespaceBuilder::default()
                .typ(LinuxNamespaceType::Pid)
                .build()
                .unwrap(),
        ])
        .uid_mappings(uid_mappings)
        .gid_mappings(gid_mappings)
        .resources(libcontainer::oci_spec::runtime::LinuxResources::default())
        .masked_paths(Vec::<String>::new())
        .readonly_paths(Vec::<String>::new())
        .build()
        .unwrap();

    SpecBuilder::default()
        .version("1.0.2")
        .root(
            RootBuilder::default()
                .path("rootfs")
                .readonly(false)
                .build()
                .unwrap(),
        )
        .process(
            ProcessBuilder::default()
                .terminal(false)
                .user(
                    UserBuilder::default()
                        .uid(0_u32)
                        .gid(0_u32)
                        .build()
                        .unwrap(),
                )
                .args(vec!["/dev/null".to_string()])
                .cwd("/")
                .capabilities(helper_capabilities())
                .no_new_privileges(false)
                .build()
                .unwrap(),
        )
        .mounts(vec![
            MountBuilder::default()
                .destination("/target")
                .typ("bind")
                .source(target_dir)
                .options(vec!["rbind".to_string(), "rw".to_string()])
                .build()
                .unwrap(),
            MountBuilder::default()
                .destination("/proc")
                .typ("proc")
                .source("proc")
                .build()
                .unwrap(),
        ])
        .linux(linux)
        .build()
        .unwrap()
}

fn helper_capabilities() -> libcontainer::oci_spec::runtime::LinuxCapabilities {
    let caps = [
        Capability::Chown,
        Capability::DacOverride,
        Capability::Fowner,
        Capability::Fsetid,
    ]
    .into_iter()
    .collect::<Capabilities>();
    LinuxCapabilitiesBuilder::default()
        .bounding(caps.clone())
        .effective(caps.clone())
        .inheritable(caps.clone())
        .permitted(caps.clone())
        .ambient(caps)
        .build()
        .unwrap()
}

fn populate_target_tree(target_dir: &Path) -> TestResult<()> {
    fs::write(target_dir.join("root-file"), "root\n")?;
    fs::write(target_dir.join("user-file"), "user\n")?;
    fs::create_dir(target_dir.join("user-dir"))?;
    std::os::unix::fs::symlink("user-file", target_dir.join("user-link"))?;
    Ok(())
}

fn assert_target_tree(target_dir: &Path, idmap: HostIdmap) -> TestResult<()> {
    assert_owner_and_mode(target_dir.join("root-file"), idmap.euid, idmap.egid, 0o644)?;
    assert_owner_and_mode(
        target_dir.join("user-file"),
        idmap.subuid_base,
        idmap.subgid_base,
        0o600,
    )?;
    assert_owner_and_mode(
        target_dir.join("user-dir"),
        idmap.subuid_base,
        idmap.subgid_base,
        0o700,
    )?;

    let symlink = fs::symlink_metadata(target_dir.join("user-link"))?;
    assert!(symlink.file_type().is_symlink());
    assert_eq!(symlink.uid(), idmap.subuid_base);
    assert_eq!(symlink.gid(), idmap.subgid_base);
    assert_eq!(
        fs::read_link(target_dir.join("user-link"))?,
        PathBuf::from("user-file")
    );
    Ok(())
}

fn assert_owner_and_mode(path: impl AsRef<Path>, uid: u32, gid: u32, mode: u32) -> TestResult<()> {
    let metadata = fs::symlink_metadata(path.as_ref())?;
    assert_eq!(metadata.uid(), uid);
    assert_eq!(metadata.gid(), gid);
    assert_eq!(metadata.permissions().mode() & 0o7777, mode);
    Ok(())
}

fn ensure_file(path: impl AsRef<Path>) -> Result<(), ExecutorError> {
    let path = path.as_ref();
    if path.is_file() {
        Ok(())
    } else {
        Err(ExecutorError::Other(format!(
            "expected file '{}'",
            path.display()
        )))
    }
}

fn ensure_directory(path: impl AsRef<Path>) -> Result<(), ExecutorError> {
    let path = path.as_ref();
    if path.is_dir() {
        Ok(())
    } else {
        Err(ExecutorError::Other(format!(
            "expected directory '{}'",
            path.display()
        )))
    }
}

fn ensure_symlink(path: impl AsRef<Path>) -> Result<(), ExecutorError> {
    let path = path.as_ref();
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(()),
        _ => Err(ExecutorError::Other(format!(
            "expected symlink '{}'",
            path.display()
        ))),
    }
}

fn chmod(path: impl AsRef<Path>, mode: u32) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn lchown(path: impl AsRef<Path>, uid: u32, gid: u32) -> std::io::Result<()> {
    let path = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes())?;
    let result = unsafe { libc::lchown(path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn executor_error(error: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Other(error.to_string())
}

fn write_report(report: &ExecutorErrorReport) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(report)?;
    fs::write("/error.json", bytes)
}

fn read_report(path: &Path) -> TestResult<ExecutorErrorReport> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn first_subid_range(path: &Path, username: &str) -> TestResult<(u32, u32)> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts = line.split(':').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(format!("malformed {} line {}", path.display(), index + 1).into());
        }
        if parts[0] != username {
            continue;
        }
        let base = parts[1].parse::<u32>()?;
        let count = parts[2].parse::<u32>()?;
        if count == 0 {
            return Err(format!("{} line {} has zero count", path.display(), index + 1).into());
        }
        base.checked_add(count - 1)
            .ok_or_else(|| format!("{} line {} overflows u32", path.display(), index + 1))?;
        return Ok((base, count));
    }
    Err(format!(
        "{} has no subid range for user '{username}'",
        path.display()
    )
    .into())
}

fn remove_dir_if_exists(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}
