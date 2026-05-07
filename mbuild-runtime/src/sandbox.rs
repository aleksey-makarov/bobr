//! Runtime-backed sandbox build execution.

use crate::bundle::{Bundle, create_bundle};
use crate::error::RuntimeError;
use crate::executor::{read_executor_result_report, write_executor_result_report};
use crate::idmap::MbuildIdmap;
use crate::preflight::preflight_ownership_runtime;
use fsobj_hash::{ObjectHash, hash_path};
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::oci_spec::runtime::{
    Capabilities, Capability, LinuxBuilder, LinuxCapabilities, LinuxCapabilitiesBuilder,
    LinuxIdMapping, LinuxIdMappingBuilder, LinuxNamespaceBuilder, LinuxNamespaceType,
    LinuxResources, Mount, MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder,
    UserBuilder,
};
use libcontainer::syscall::syscall::SyscallType;
use libcontainer::workload::{
    Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Gid, Uid, chown};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tracing::warn;
use uuid::Uuid;

const BUILD_USER_UID: u32 = 1;
const BUILD_USER_GID: u32 = 1;
const ROOT_UID: u32 = 0;
const ROOT_GID: u32 = 0;
const ROOT_STEP_CAPABILITIES: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
];

/// User identity used for a sandbox step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRunAs {
    /// Run as the sandbox build user, currently numeric `1:1`.
    BuildUser,
    /// Run as container root, currently numeric `0:0`.
    Root,
}

/// A named input mount passed to a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxInput {
    /// Recipe input name.
    pub name: String,
    /// Host path to the realized input object.
    pub host_path: PathBuf,
    /// Absolute mount path inside the sandbox.
    pub mount_path: PathBuf,
}

/// One command step to execute inside a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxStep {
    /// Step name.
    pub name: String,
    /// User identity for the step.
    pub run_as: SandboxRunAs,
    /// Absolute working directory inside the sandbox.
    pub cwd: PathBuf,
    /// Argument vector to execute.
    pub argv: Vec<String>,
    /// Environment variables for the step.
    pub env: HashMap<String, String>,
    /// Host log path for stdout.
    pub stdout_path: PathBuf,
    /// Host log path for stderr.
    pub stderr_path: PathBuf,
}

/// Runtime configuration for one sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxBuildConfig {
    /// Realized rootfs object path.
    pub rootfs: PathBuf,
    /// Host directory for build output.
    pub out_dir: PathBuf,
    /// Host directory for script config.
    pub config_dir: PathBuf,
    /// Host workspace for temporary runtime state.
    pub workspace: PathBuf,
    /// Persistent root for libcontainer state.
    pub state_dir: PathBuf,
    /// Additional named inputs.
    pub inputs: Vec<SandboxInput>,
    /// Ordered build steps.
    pub steps: Vec<SandboxStep>,
}

/// Result of a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxBuildOutcome {
    /// Runtime-side output object hash.
    pub object_hash: ObjectHash,
    /// Structured per-step reports.
    pub steps: Vec<SandboxStepReport>,
}

/// Structured report for one sandbox step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxStepReport {
    /// Step name.
    pub name: String,
    /// Step user identity.
    pub run_as: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Step duration in milliseconds.
    pub duration_ms: u128,
    /// Host stdout log path.
    pub stdout_path: PathBuf,
    /// Host stderr log path.
    pub stderr_path: PathBuf,
}

/// Execute a complete sandbox build and return the output hash.
pub fn run_sandbox_build(
    config: SandboxBuildConfig,
    idmap: &MbuildIdmap,
) -> Result<SandboxBuildOutcome, RuntimeError> {
    validate_config(&config)?;
    preflight_ownership_runtime(idmap)?;

    let prepared = PreparedSandbox::create(&config, idmap)?;
    let mut lifecycle = SandboxLifecycle::start(
        prepared.bundle,
        prepared.output_hash_path,
        &config.state_dir,
    )?;
    let result = run_sandbox_build_inner(&mut lifecycle, &config);
    let cleanup = lifecycle.cleanup();

    match (result, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(outcome), Err(error)) => {
            warn!("failed to cleanup sandbox runtime after successful build: {error}");
            Ok(outcome)
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            warn!("failed to cleanup sandbox runtime after failed build: {cleanup_error}");
            Err(error)
        }
    }
}

fn run_sandbox_build_inner(
    lifecycle: &mut SandboxLifecycle,
    config: &SandboxBuildConfig,
) -> Result<SandboxBuildOutcome, RuntimeError> {
    lifecycle.exec_prepare(&config.inputs)?;

    let mut reports = Vec::new();
    for step in &config.steps {
        reports.push(lifecycle.exec_step(step)?);
    }

    let object_hash = lifecycle.hash_output()?;
    Ok(SandboxBuildOutcome {
        object_hash,
        steps: reports,
    })
}

fn validate_config(config: &SandboxBuildConfig) -> Result<(), RuntimeError> {
    require_directory(&config.rootfs, "sandbox rootfs")?;
    require_directory(&config.out_dir, "sandbox output directory")?;
    require_directory(&config.config_dir, "sandbox config directory")?;
    require_directory(&config.workspace, "sandbox workspace")?;
    require_directory(&config.state_dir, "sandbox state directory")?;
    for input in &config.inputs {
        if input.mount_path.is_relative() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox input '{}' mount path must be absolute: '{}'",
                input.name,
                input.mount_path.display()
            )));
        }
        if !input.host_path.is_dir() && !input.host_path.is_file() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox input '{}' must resolve to a file or directory: '{}'",
                input.name,
                input.host_path.display()
            )));
        }
    }
    Ok(())
}

struct PreparedSandbox {
    bundle: Bundle,
    output_hash_path: PathBuf,
}

impl PreparedSandbox {
    fn create(config: &SandboxBuildConfig, idmap: &MbuildIdmap) -> Result<Self, RuntimeError> {
        let mut dirs = SandboxDirs::create(&config.workspace)?;
        let host_files = SandboxHostFiles::create(&dirs.host_files)?;
        let spec = build_sandbox_spec(config, idmap, &mut dirs, &host_files)?;
        let bundle = create_bundle(&config.workspace, &spec)?;
        Ok(Self {
            bundle,
            output_hash_path: host_files.output_hash,
        })
    }
}

struct SandboxDirs {
    root_upper: PathBuf,
    root_work: PathBuf,
    input_overlays: HashMap<String, InputOverlayDirs>,
    host_files: PathBuf,
}

struct InputOverlayDirs {
    upper: PathBuf,
    work: PathBuf,
}

impl SandboxDirs {
    fn create(workspace: &Path) -> Result<Self, RuntimeError> {
        let root = workspace
            .join("sandbox")
            .join(Uuid::new_v4().simple().to_string());
        let root_upper = root.join("rootfs-upper");
        let root_work = root.join("rootfs-work");
        let inputs = root.join("inputs");
        let host_files = root.join("host-files");

        fs::create_dir_all(&root_upper)?;
        fs::create_dir_all(&root_work)?;
        fs::create_dir_all(&inputs)?;
        fs::create_dir_all(&host_files)?;

        Ok(Self {
            root_upper,
            root_work,
            input_overlays: HashMap::new(),
            host_files,
        })
    }

    fn input_overlay(&mut self, name: &str) -> Result<&InputOverlayDirs, RuntimeError> {
        if !self.input_overlays.contains_key(name) {
            let root = self
                .root_work
                .parent()
                .expect("root work has parent")
                .join("inputs")
                .join(name);
            let upper = root.join("upper");
            let work = root.join("work");
            fs::create_dir_all(&upper)?;
            fs::create_dir_all(&work)?;
            self.input_overlays
                .insert(name.to_string(), InputOverlayDirs { upper, work });
        }
        Ok(self.input_overlays.get(name).expect("input overlay exists"))
    }
}

struct SandboxHostFiles {
    hosts: PathBuf,
    resolv_conf: PathBuf,
    output_hash: PathBuf,
}

impl SandboxHostFiles {
    fn create(root: &Path) -> Result<Self, RuntimeError> {
        let hosts = root.join("hosts");
        let resolv_conf = root.join("resolv.conf");
        let output_hash = root.join("output-hash.json");
        fs::write(&hosts, "127.0.0.1 localhost mbuild\n::1 localhost mbuild\n")?;
        fs::write(&resolv_conf, "")?;
        File::create(&output_hash)?;
        Ok(Self {
            hosts,
            resolv_conf,
            output_hash,
        })
    }
}

struct SandboxLifecycle {
    container: libcontainer::container::Container,
    container_id: String,
    state_dir: PathBuf,
    _bundle: Bundle,
    output_hash_path: PathBuf,
}

impl SandboxLifecycle {
    fn start(
        bundle: Bundle,
        output_hash_path: PathBuf,
        state_dir: &Path,
    ) -> Result<Self, RuntimeError> {
        let container_id = format!("mbuild-sandbox-{}", Uuid::new_v4().simple());
        let mut container = ContainerBuilder::new(container_id.clone(), SyscallType::Linux)
            .with_executor(KeepAliveExecutor)
            .with_root_path(state_dir)
            .map_err(libcontainer_error)?
            .as_init(bundle.dir())
            .with_systemd(false)
            .with_detach(true)
            .build()
            .map_err(libcontainer_error)?;
        container.start().map_err(libcontainer_error)?;

        Ok(Self {
            container,
            container_id,
            state_dir: state_dir.to_path_buf(),
            _bundle: bundle,
            output_hash_path,
        })
    }

    fn exec_prepare(&self, inputs: &[SandboxInput]) -> Result<(), RuntimeError> {
        let mut paths = vec![PathBuf::from("/__mbuild/build")];
        paths.extend(
            inputs
                .iter()
                .filter(|input| input.host_path.is_dir())
                .map(|input| input.mount_path.clone()),
        );
        self.exec_custom(
            "sandbox-prepare",
            PrepareExecutor {
                paths,
                uid: BUILD_USER_UID,
                gid: BUILD_USER_GID,
            },
            ROOT_UID,
            ROOT_GID,
            root_capabilities(),
            None,
        )
        .map(|_| ())
    }

    fn exec_step(&self, step: &SandboxStep) -> Result<SandboxStepReport, RuntimeError> {
        let start = Instant::now();
        let (uid, gid, capabilities) = match step.run_as {
            SandboxRunAs::BuildUser => (BUILD_USER_UID, BUILD_USER_GID, Vec::new()),
            SandboxRunAs::Root => (ROOT_UID, ROOT_GID, root_capabilities()),
        };
        let exit_code = self.exec_default(
            &step.name,
            uid,
            gid,
            capabilities,
            Some(step.cwd.clone()),
            step.argv.clone(),
            step_env(step),
            Some((&step.stdout_path, &step.stderr_path)),
        )?;
        Ok(SandboxStepReport {
            name: step.name.clone(),
            run_as: match step.run_as {
                SandboxRunAs::BuildUser => "build-user".to_string(),
                SandboxRunAs::Root => "root".to_string(),
            },
            exit_code,
            duration_ms: start.elapsed().as_millis(),
            stdout_path: step.stdout_path.clone(),
            stderr_path: step.stderr_path.clone(),
        })
    }

    fn hash_output(&self) -> Result<ObjectHash, RuntimeError> {
        self.exec_custom(
            "sandbox-hash-output",
            HashExecutor {
                path: PathBuf::from("/__mbuild/out"),
                result_path: PathBuf::from("/__mbuild/runtime/output-hash.json"),
            },
            ROOT_UID,
            ROOT_GID,
            root_capabilities(),
            None,
        )?;
        read_executor_result_report(&self.output_hash_path)?.ok_or_else(|| {
            RuntimeError::Executor(format!(
                "sandbox output hash report '{}' is empty",
                self.output_hash_path.display()
            ))
        })
    }

    fn exec_custom<E>(
        &self,
        label: &str,
        executor: E,
        uid: u32,
        gid: u32,
        capabilities: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> Result<i32, RuntimeError>
    where
        E: Executor + 'static,
    {
        let builder = ContainerBuilder::new(self.container_id.clone(), SyscallType::Linux)
            .with_executor(executor)
            .with_root_path(&self.state_dir)
            .map_err(libcontainer_error)?
            .as_tenant()
            .with_user(Some(uid))
            .with_group(Some(gid))
            .with_no_new_privs(true)
            .with_capabilities(capabilities)
            .with_cwd(cwd)
            .with_detach(false);

        let pid = builder.build().map_err(libcontainer_error)?;
        wait_for_tenant(label, pid)
    }

    fn exec_default(
        &self,
        label: &str,
        uid: u32,
        gid: u32,
        capabilities: Vec<String>,
        cwd: Option<PathBuf>,
        argv: Vec<String>,
        env: HashMap<String, String>,
        logs: Option<(&Path, &Path)>,
    ) -> Result<i32, RuntimeError> {
        let mut base = ContainerBuilder::new(self.container_id.clone(), SyscallType::Linux)
            .with_root_path(&self.state_dir)
            .map_err(libcontainer_error)?;
        if let Some((stdout, stderr)) = logs {
            base = with_stdio_logs(base, stdout, stderr)?;
        }
        let builder = base
            .as_tenant()
            .with_container_args(argv)
            .with_env(env)
            .with_cwd(cwd)
            .with_user(Some(uid))
            .with_group(Some(gid))
            .with_no_new_privs(true)
            .with_capabilities(capabilities)
            .with_detach(false);
        let pid = builder.build().map_err(libcontainer_error)?;
        wait_for_tenant(label, pid)
    }

    fn cleanup(&mut self) -> Result<(), RuntimeError> {
        self.container.delete(true).map_err(libcontainer_error)
    }
}

fn with_stdio_logs(
    builder: ContainerBuilder,
    stdout: &Path,
    stderr: &Path,
) -> Result<ContainerBuilder, RuntimeError> {
    let stdout = File::create(stdout)?;
    let stderr = File::create(stderr)?;
    Ok(builder.with_stdout(stdout).with_stderr(stderr))
}

fn wait_for_tenant(label: &str, pid: nix::unistd::Pid) -> Result<i32, RuntimeError> {
    match waitpid(pid, None).map_err(libcontainer_error)? {
        WaitStatus::Exited(_, code) => {
            if code == 0 {
                Ok(code)
            } else {
                Err(RuntimeError::Executor(format!(
                    "sandbox step '{label}' failed with exit status {code}"
                )))
            }
        }
        status => Err(RuntimeError::Executor(format!(
            "sandbox step '{label}' ended with wait status {status:?}"
        ))),
    }
}

fn build_sandbox_spec(
    config: &SandboxBuildConfig,
    idmap: &MbuildIdmap,
    dirs: &mut SandboxDirs,
    host_files: &SandboxHostFiles,
) -> Result<Spec, RuntimeError> {
    let uid_mappings = vec![
        linux_id_mapping(0, idmap.current_uid(), 1)?,
        linux_id_mapping(1, idmap.subuid_base(), idmap.subuid_count())?,
    ];
    let gid_mappings = vec![
        linux_id_mapping(0, idmap.current_gid(), 1)?,
        linux_id_mapping(1, idmap.subgid_base(), idmap.subgid_count())?,
    ];

    let linux = build_oci(
        LinuxBuilder::default()
            .namespaces(vec![
                namespace(LinuxNamespaceType::User)?,
                namespace(LinuxNamespaceType::Mount)?,
                namespace(LinuxNamespaceType::Pid)?,
                namespace(LinuxNamespaceType::Uts)?,
                namespace(LinuxNamespaceType::Ipc)?,
                namespace(LinuxNamespaceType::Network)?,
            ])
            .uid_mappings(uid_mappings)
            .gid_mappings(gid_mappings)
            .resources(LinuxResources::default())
            .masked_paths(Vec::<String>::new())
            .readonly_paths(Vec::<String>::new())
            .build(),
    )?;

    let mut mounts = vec![
        overlay_mount(
            Path::new("/"),
            &config.rootfs,
            &dirs.root_upper,
            &dirs.root_work,
        )?,
        proc_mount()?,
        tmpfs_mount(Path::new("/tmp"), &["mode=1777"])?,
        tmpfs_mount(Path::new("/run"), &["mode=755"])?,
        bind_mount(&config.config_dir, Path::new("/__mbuild/config"), true)?,
        bind_mount(&config.out_dir, Path::new("/__mbuild/out"), false)?,
        bind_mount(&host_files.hosts, Path::new("/etc/hosts"), true)?,
        bind_mount(&host_files.resolv_conf, Path::new("/etc/resolv.conf"), true)?,
        bind_mount(
            &host_files.output_hash,
            Path::new("/__mbuild/runtime/output-hash.json"),
            false,
        )?,
    ];

    for input in &config.inputs {
        if input.host_path.is_dir() {
            let overlay = dirs.input_overlay(&input.name)?;
            mounts.push(overlay_mount(
                &input.mount_path,
                &input.host_path,
                &overlay.upper,
                &overlay.work,
            )?);
        } else {
            mounts.push(bind_mount(&input.host_path, &input.mount_path, true)?);
        }
    }

    build_oci(
        SpecBuilder::default()
            .version("1.0.2")
            .hostname("mbuild")
            .root(build_oci(
                RootBuilder::default()
                    .path("rootfs")
                    .readonly(false)
                    .build(),
            )?)
            .process(build_oci(
                ProcessBuilder::default()
                    .terminal(false)
                    .user(build_oci(
                        UserBuilder::default().uid(0_u32).gid(0_u32).build(),
                    )?)
                    .args(vec!["mbuild-sandbox-init".to_string()])
                    .cwd("/")
                    .capabilities(root_linux_capabilities()?)
                    .no_new_privileges(true)
                    .build(),
            )?)
            .mounts(mounts)
            .linux(linux)
            .build(),
    )
}

fn overlay_mount(
    destination: &Path,
    lower: &Path,
    upper: &Path,
    work: &Path,
) -> Result<Mount, RuntimeError> {
    build_oci(
        MountBuilder::default()
            .destination(destination)
            .typ("overlay")
            .source(Path::new("overlay"))
            .options(vec![
                format!("lowerdir={}", lower.display()),
                format!("upperdir={}", upper.display()),
                format!("workdir={}", work.display()),
                "userxattr".to_string(),
            ])
            .build(),
    )
}

fn bind_mount(source: &Path, destination: &Path, readonly: bool) -> Result<Mount, RuntimeError> {
    let mut options = vec!["rbind".to_string()];
    if readonly {
        options.push("ro".to_string());
    } else {
        options.push("rw".to_string());
    }
    build_oci(
        MountBuilder::default()
            .destination(destination)
            .typ("bind")
            .source(source)
            .options(options)
            .build(),
    )
}

fn proc_mount() -> Result<Mount, RuntimeError> {
    build_oci(
        MountBuilder::default()
            .destination(Path::new("/proc"))
            .typ("proc")
            .source(Path::new("proc"))
            .build(),
    )
}

fn tmpfs_mount(destination: &Path, extra_options: &[&str]) -> Result<Mount, RuntimeError> {
    let mut options = vec![
        "nosuid".to_string(),
        "nodev".to_string(),
        "noexec".to_string(),
    ];
    options.extend(extra_options.iter().map(|option| option.to_string()));
    build_oci(
        MountBuilder::default()
            .destination(destination)
            .typ("tmpfs")
            .source(Path::new("tmpfs"))
            .options(options)
            .build(),
    )
}

fn namespace(
    typ: LinuxNamespaceType,
) -> Result<libcontainer::oci_spec::runtime::LinuxNamespace, RuntimeError> {
    build_oci(LinuxNamespaceBuilder::default().typ(typ).build())
}

fn linux_id_mapping(
    container_id: u32,
    host_id: u32,
    size: u32,
) -> Result<LinuxIdMapping, RuntimeError> {
    build_oci(
        LinuxIdMappingBuilder::default()
            .container_id(container_id)
            .host_id(host_id)
            .size(size)
            .build(),
    )
}

fn root_linux_capabilities() -> Result<LinuxCapabilities, RuntimeError> {
    let caps = [
        Capability::Chown,
        Capability::DacOverride,
        Capability::DacReadSearch,
        Capability::Fowner,
        Capability::Fsetid,
    ]
    .into_iter()
    .collect::<Capabilities>();

    build_oci(
        LinuxCapabilitiesBuilder::default()
            .bounding(caps.clone())
            .effective(caps.clone())
            .inheritable(caps.clone())
            .permitted(caps.clone())
            .ambient(caps)
            .build(),
    )
}

fn root_capabilities() -> Vec<String> {
    ROOT_STEP_CAPABILITIES
        .iter()
        .map(|capability| capability.to_string())
        .collect()
}

fn step_env(step: &SandboxStep) -> HashMap<String, String> {
    let mut env = HashMap::from([
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        ("HOME".to_string(), "/__mbuild/build".to_string()),
        ("USER".to_string(), "mbuild".to_string()),
        (
            "MBUILD_CONFIG_DIR".to_string(),
            "/__mbuild/config".to_string(),
        ),
        (
            "MBUILD_BUILD_DIR".to_string(),
            "/__mbuild/build".to_string(),
        ),
        ("MBUILD_OUT_DIR".to_string(), "/__mbuild/out".to_string()),
        ("MBUILD_STEP_NAME".to_string(), step.name.clone()),
    ]);
    env.extend(step.env.clone());
    env
}

#[derive(Clone)]
struct KeepAliveExecutor;

impl Executor for KeepAliveExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
}

#[derive(Clone)]
struct PrepareExecutor {
    paths: Vec<PathBuf>,
    uid: u32,
    gid: u32,
}

impl Executor for PrepareExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        if let Err(error) = fs::remove_file("/dev/tty")
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(executor_error(error));
        }
        for path in &self.paths {
            fs::create_dir_all(path).map_err(executor_error)?;
            chown_tree(path, self.uid, self.gid).map_err(executor_error)?;
        }
        std::process::exit(0)
    }
}

#[derive(Clone)]
struct HashExecutor {
    path: PathBuf,
    result_path: PathBuf,
}

impl Executor for HashExecutor {
    fn setup_envs(&self, _: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        Ok(())
    }

    fn validate(&self, _: &Spec) -> Result<(), ExecutorValidationError> {
        Ok(())
    }

    fn exec(&self, _: &Spec) -> Result<(), ExecutorError> {
        let object_hash = hash_path(&self.path).map_err(executor_error)?;
        write_executor_result_report(&self.result_path, object_hash)?;
        std::process::exit(0)
    }
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        lchown(path, uid, gid)?;
        return Ok(());
    }

    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(io::Error::other)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let result = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn require_directory(path: &Path, label: &str) -> Result<(), RuntimeError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "{label} '{}' must exist and be a directory",
            path.display()
        )))
    }
}

fn build_oci<T>(result: Result<T, impl std::fmt::Display>) -> Result<T, RuntimeError> {
    result.map_err(|error| RuntimeError::Libcontainer(error.to_string()))
}

fn libcontainer_error(error: impl Error) -> RuntimeError {
    RuntimeError::Libcontainer(format_error_chain(&error))
}

fn format_error_chain(error: &dyn Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

fn executor_error(error: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sandbox_spec_uses_rootfs_overlay_and_output_bind() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        for path in [&rootfs, &out, &config, &workspace, &state] {
            fs::create_dir_all(path).unwrap();
        }
        let build_config = SandboxBuildConfig {
            rootfs: rootfs.clone(),
            out_dir: out.clone(),
            config_dir: config,
            workspace: workspace.clone(),
            state_dir: state,
            inputs: Vec::new(),
            steps: Vec::new(),
        };
        let mut dirs = SandboxDirs::create(&workspace).unwrap();
        let host_files = SandboxHostFiles::create(&dirs.host_files).unwrap();
        let idmap = MbuildIdmap::for_tests(1000, 1001, 100000, 65536, 200000, 65536);

        let spec = build_sandbox_spec(&build_config, &idmap, &mut dirs, &host_files).unwrap();
        let mounts = spec.mounts().as_ref().unwrap();

        let root_overlay = mounts
            .iter()
            .find(|mount| {
                mount.destination() == Path::new("/") && mount.typ().as_deref() == Some("overlay")
            })
            .expect("root overlay mount exists");
        let root_overlay_options = root_overlay.options().as_ref().unwrap();
        assert!(
            root_overlay_options
                .iter()
                .any(|option| option == "userxattr")
        );
        assert!(
            !root_overlay_options
                .iter()
                .any(|option| option == "metacopy=on")
        );
        assert!(
            mounts
                .iter()
                .any(|mount| mount.destination() == Path::new("/__mbuild/out")
                    && mount.source().as_deref() == Some(out.as_path()))
        );
    }
}
