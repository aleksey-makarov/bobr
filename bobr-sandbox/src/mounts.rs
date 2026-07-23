use crate::{SandboxInput, SandboxRuntimeStep, StepUser};
use bobr_runtime::runtime::RuntimeError;
use bobr_sandbox_launcher::{
    CONTAINER_BUILD_DIR, CONTAINER_CONFIG_DIR, CONTAINER_FAILURE_REPORT, CONTAINER_INPUTS_DIR,
    CONTAINER_LAUNCHER_DIR, CONTAINER_LOG_DIR, CONTAINER_OUT_DIR, CONTAINER_RUNNER_CONFIG,
    CONTAINER_RUNTIME_DIR, CONTAINER_SUCCESS_REPORT, LAUNCHER_BINARY_NAME, RunnerConfig,
    RunnerRunAs, RunnerStepConfig, SANDBOX_PROTOCOL_VERSION, SandboxLauncherConfig,
    SandboxLauncherMount, SandboxLauncherMountKind, SandboxRootOverlay, validate_launcher_config,
};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

pub(crate) struct PreparedSandbox {
    pub(crate) runtime_files: SandboxRuntimeFiles,
    pub(crate) launcher_config: PathBuf,
    /// Overlay upper layer: the build output is captured from here.
    pub(crate) upper: PathBuf,
}

impl PreparedSandbox {
    pub(crate) fn create(
        config: &SandboxInput,
        workspace: &Path,
        out_dir: Option<&Path>,
    ) -> Result<Self, RuntimeError> {
        validate_config(config, workspace)?;
        let dirs = SandboxDirs::create(workspace)?;
        let runtime_files = SandboxRuntimeFiles::create(&dirs.runtime_files, config)?;
        write_runner_config(config, &runtime_files)?;
        populate_overlay_upper(&dirs.upper)?;
        let launcher_config = dirs.root.join("launcher-config.json");
        let launcher = build_launcher_config(config, &dirs, &runtime_files, out_dir)?;
        serde_json::to_writer(File::create(&launcher_config)?, &launcher)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        Ok(Self {
            runtime_files,
            launcher_config,
            upper: dirs.upper.clone(),
        })
    }
}

pub(crate) struct SandboxDirs {
    pub(crate) root: PathBuf,
    rootfs: PathBuf,
    build_dir: PathBuf,
    runtime_files: PathBuf,
    /// Overlay upper layer: every write to the root lands here, and this is the
    /// layer the build output is captured from.
    upper: PathBuf,
    /// Overlay work directory (kernel-owned scratch, same fs as `upper`).
    work: PathBuf,
}

impl SandboxDirs {
    fn create(workspace: &Path) -> Result<Self, RuntimeError> {
        let root = workspace.join("sandbox");
        let rootfs = root.join("rootfs");
        let build_dir = root.join("build");
        let runtime_files = root.join("runtime-files");
        let upper = root.join("upper");
        let work = root.join("work");

        match fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(RuntimeError::new(error.to_string())),
        }
        fs::create_dir_all(&rootfs)?;
        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&runtime_files)?;
        fs::create_dir_all(&upper)?;
        fs::create_dir_all(&work)?;

        Ok(Self {
            root,
            rootfs,
            build_dir,
            runtime_files,
            upper,
            work,
        })
    }
}

fn validate_config(config: &SandboxInput, workspace: &Path) -> Result<(), RuntimeError> {
    require_directory(&config.rootfs, "sandbox rootfs")?;
    require_directory(&config.config_dir, "sandbox config directory")?;
    require_directory(workspace, "sandbox workspace")?;
    validate_rootfs_top_level(&config.rootfs)?;
    let mut input_names = std::collections::HashSet::new();
    for input in &config.extra_inputs {
        if !input_names.insert(input.name.as_str()) {
            return Err(RuntimeError::new(format!(
                "sandbox input '{}' is defined more than once",
                input.name
            )));
        }
        if !input.path.is_dir() && !input.path.is_file() {
            return Err(RuntimeError::new(format!(
                "sandbox input '{}' must resolve to a file or directory: '{}'",
                input.name,
                input.path.display()
            )));
        }
    }
    validate_steps(&config.steps)
}

fn require_directory(path: &Path, label: &str) -> Result<(), RuntimeError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(RuntimeError::new(format!(
            "{label} '{}' must exist and be a directory",
            path.display()
        )))
    }
}

fn validate_steps(steps: &[SandboxRuntimeStep]) -> Result<(), RuntimeError> {
    let mut names = std::collections::HashSet::new();
    let mut log_paths = std::collections::HashSet::new();
    for step in steps {
        if !names.insert(step.name.as_str()) {
            return Err(RuntimeError::new(format!(
                "sandbox step '{}' is defined more than once",
                step.name
            )));
        }
        if !step.cwd.is_absolute() {
            return Err(RuntimeError::new(format!(
                "sandbox step '{}' cwd must be absolute: '{}'",
                step.name,
                step.cwd.display()
            )));
        }
        if step.argv.is_empty() {
            return Err(RuntimeError::new(format!(
                "sandbox step '{}' argv must not be empty",
                step.name
            )));
        }
        if step.argv[0].is_empty() {
            return Err(RuntimeError::new(format!(
                "sandbox step '{}' argv[0] must not be empty",
                step.name
            )));
        }
        if step.umask > 0o777 {
            return Err(RuntimeError::new(format!(
                "sandbox step '{}' umask must be in 0o000..=0o777, got {:#o}",
                step.name, step.umask
            )));
        }
        for (stream, path) in [("stdout", &step.stdout_path), ("stderr", &step.stderr_path)] {
            if !log_paths.insert(path.clone()) {
                return Err(RuntimeError::new(format!(
                    "sandbox step '{}' {stream} log path is not unique: '{}'",
                    step.name,
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_rootfs_top_level(rootfs: &Path) -> Result<(), RuntimeError> {
    reject_reserved_rootfs_entry(rootfs, "__bobr")?;
    for entry in rootfs_top_level_entries(rootfs)? {
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            RuntimeError::new(format!(
                "sandbox rootfs '{}' contains non-UTF-8 top-level entry",
                rootfs.display()
            ))
        })?;
        let file_type = entry.file_type()?;
        if !(file_type.is_file() || file_type.is_dir() || file_type.is_symlink()) {
            return Err(RuntimeError::new(format!(
                "sandbox rootfs '{}' top-level entry '/{name}' must be a file, directory, or symlink",
                rootfs.display()
            )));
        }
    }
    Ok(())
}

fn reject_reserved_rootfs_entry(rootfs: &Path, name: &str) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(rootfs.join(name)) {
        Ok(_) => Err(RuntimeError::new(format!(
            "sandbox rootfs '{}' contains reserved top-level entry '/{name}'",
            rootfs.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::new(error.to_string())),
    }
}

fn write_runner_config(
    config: &SandboxInput,
    runtime_files: &SandboxRuntimeFiles,
) -> Result<(), RuntimeError> {
    let steps = config
        .steps
        .iter()
        .zip(runtime_files.step_logs.iter())
        .map(|(step, logs)| RunnerStepConfig {
            name: step.name.clone(),
            run_as: match step.run_as {
                StepUser::BuildUser => RunnerRunAs::BuildUser,
                StepUser::Root => RunnerRunAs::Root,
            },
            cwd: step.cwd.clone(),
            argv: step.argv.clone(),
            env: effective_step_env(step, &config.build_seed_hex, config.plain_object),
            umask: step.umask,
            stdout_path: logs.container_stdout.clone(),
            stderr_path: logs.container_stderr.clone(),
            report_stdout_path: step.stdout_path.clone(),
            report_stderr_path: step.stderr_path.clone(),
        })
        .collect::<Vec<_>>();
    let runner_config = RunnerConfig {
        protocol_version: SANDBOX_PROTOCOL_VERSION,
        prepare_paths: vec![PathBuf::from(CONTAINER_BUILD_DIR)],
        steps,
        success_report: PathBuf::from(CONTAINER_SUCCESS_REPORT),
        failure_report: PathBuf::from(CONTAINER_FAILURE_REPORT),
    };
    serde_json::to_writer(File::create(&runtime_files.runner_config)?, &runner_config)
        .map_err(|error| RuntimeError::new(error.to_string()))
}

pub(crate) struct SandboxRuntimeFiles {
    root: PathBuf,
    pub(crate) success_report: PathBuf,
    pub(crate) failure_report: PathBuf,
    runner_config: PathBuf,
    step_logs: Vec<SandboxStepLogMounts>,
}

impl SandboxRuntimeFiles {
    fn create(root: &Path, config: &SandboxInput) -> Result<Self, RuntimeError> {
        fs::create_dir_all(root)?;
        let success_report = root.join("sandbox-success.json");
        let failure_report = root.join("sandbox-failure.json");
        let runner_config = root.join("runner-config.json");
        File::create(&success_report)?;
        File::create(&failure_report)?;
        File::create(&runner_config)?;
        fs::create_dir_all(root.join("logs"))?;
        let mut step_logs = Vec::new();
        for (index, step) in config.steps.iter().enumerate() {
            File::create(&step.stdout_path)?;
            File::create(&step.stderr_path)?;
            step_logs.push(SandboxStepLogMounts {
                host_stdout: step.stdout_path.clone(),
                host_stderr: step.stderr_path.clone(),
                container_stdout: Path::new(CONTAINER_LOG_DIR).join(format!("{index}.stdout")),
                container_stderr: Path::new(CONTAINER_LOG_DIR).join(format!("{index}.stderr")),
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
            success_report,
            failure_report,
            runner_config,
            step_logs,
        })
    }
}

struct SandboxStepLogMounts {
    host_stdout: PathBuf,
    host_stderr: PathBuf,
    container_stdout: PathBuf,
    container_stderr: PathBuf,
}

fn build_launcher_config(
    config: &SandboxInput,
    dirs: &SandboxDirs,
    runtime_files: &SandboxRuntimeFiles,
    out_dir: Option<&Path>,
) -> Result<SandboxLauncherConfig, RuntimeError> {
    // The root itself is an overlay (see root_overlay below), so the rootfs is
    // no longer bound entry by entry; these mounts only add things inside it.
    let mut mounts: Vec<SandboxLauncherMount> = Vec::new();
    mounts.extend([
        bind_mount(Path::new("/dev/null"), Path::new("/dev/null"), false),
        bind_mount(Path::new("/dev/zero"), Path::new("/dev/zero"), false),
        bind_mount(Path::new("/dev/full"), Path::new("/dev/full"), false),
        bind_mount(Path::new("/dev/random"), Path::new("/dev/random"), false),
        bind_mount(Path::new("/dev/urandom"), Path::new("/dev/urandom"), false),
        proc_mount(Path::new("/proc")),
        tmpfs_mount(Path::new("/tmp"), false, &["mode=1777"]),
        tmpfs_mount(Path::new("/run"), true, &["mode=755"]),
        // POSIX named semaphores (sem_open) and POSIX shared memory live in
        // /dev/shm; Python's multiprocessing (ProcessPoolExecutor) and its
        // configure-time semaphore probe need it, and it is a standard part of
        // a Linux runtime a build may reasonably expect.
        tmpfs_mount(Path::new("/dev/shm"), false, &["mode=1777"]),
        bind_mount(&dirs.build_dir, Path::new(CONTAINER_BUILD_DIR), false),
        bind_mount(&config.config_dir, Path::new(CONTAINER_CONFIG_DIR), true),
        bind_mount(
            &config.launcher_path,
            &Path::new(CONTAINER_LAUNCHER_DIR).join(LAUNCHER_BINARY_NAME),
            true,
        ),
        bind_mount(&runtime_files.root, Path::new(CONTAINER_RUNTIME_DIR), false),
    ]);

    // The plain-object `Sandbox` path installs into `$out` (writable), captured
    // as a standalone object; the additive path has no `$out` and captures the
    // overlay upper instead.
    if let Some(out_dir) = out_dir {
        mounts.push(bind_mount(out_dir, Path::new(CONTAINER_OUT_DIR), false));
    }

    for log in &runtime_files.step_logs {
        mounts.push(bind_mount(&log.host_stdout, &log.container_stdout, false));
        mounts.push(bind_mount(&log.host_stderr, &log.container_stderr, false));
    }

    for input in &config.extra_inputs {
        mounts.push(bind_mount(
            &input.path,
            &input_mount_path(&input.name),
            true,
        ));
    }
    let launcher = SandboxLauncherConfig {
        protocol_version: SANDBOX_PROTOCOL_VERSION,
        root: dirs.rootfs.clone(),
        mounts,
        runner_config: PathBuf::from(CONTAINER_RUNNER_CONFIG),
        // Host path by design: the launcher opens this before chroot and
        // writes launcher-level failures through that fd afterwards.
        failure_report: runtime_files.failure_report.clone(),
        // The root is a read-write overlay: the input rootfs is the read-only
        // lower layer, and every write to the root lands in `upper`, which the
        // build output is captured from.
        root_overlay: Some(SandboxRootOverlay {
            lower: config.rootfs.clone(),
            upper: dirs.upper.clone(),
            work: dirs.work.clone(),
        }),
    };
    validate_launcher_config(&launcher).map_err(|error| RuntimeError::new(error.to_string()))?;
    Ok(launcher)
}

fn rootfs_top_level_entries(rootfs: &Path) -> Result<Vec<fs::DirEntry>, RuntimeError> {
    fs::read_dir(rootfs)?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(RuntimeError::from)
}

/// Seeds the overlay upper layer with the runtime-only entries that are neither
/// part of the lower rootfs nor themselves mounts: the `/dev` convenience
/// symlinks. Everything else in the merged root comes from the lower layer, and
/// every mount target (mountpoint dirs, the launcher and step-log files) is
/// created by the launcher when it applies the interior mounts.
fn populate_overlay_upper(upper: &Path) -> Result<(), RuntimeError> {
    fs::create_dir_all(upper.join("dev"))?;
    create_dev_symlink(upper, "fd", "/proc/self/fd")?;
    create_dev_symlink(upper, "stdin", "/proc/self/fd/0")?;
    create_dev_symlink(upper, "stdout", "/proc/self/fd/1")?;
    create_dev_symlink(upper, "stderr", "/proc/self/fd/2")?;
    Ok(())
}

fn create_dev_symlink(root: &Path, name: &str, target: &str) -> Result<(), RuntimeError> {
    let link = root.join("dev").join(name);
    if !link.exists() && !link.is_symlink() {
        symlink(target, link)?;
    }
    Ok(())
}

/// Top-level entries the sandbox itself injects into the overlay upper: the
/// mount points for the interior mounts (all under `/__bobr`, plus `/dev`,
/// `/proc`, `/run`, `/tmp`) and the seeded `/dev` symlinks. None of them are
/// part of the build's output.
const OVERLAY_SCAFFOLDING_TOP_LEVEL: &[&str] = &["__bobr", "dev", "proc", "run", "tmp"];

/// Removes the sandbox's own scaffolding from the overlay upper, leaving only
/// what the build itself wrote, so the captured layer holds no mount points or
/// seeded `/dev` symlinks.
pub(crate) fn strip_overlay_scaffolding(upper: &Path) -> Result<(), RuntimeError> {
    for name in OVERLAY_SCAFFOLDING_TOP_LEVEL {
        let path = upper.join(name);
        match fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(RuntimeError::new(format!(
                    "remove overlay scaffolding '{}': {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn input_mount_path(name: &str) -> PathBuf {
    Path::new(CONTAINER_INPUTS_DIR).join(name)
}

fn bind_mount(source: &Path, target: &Path, readonly: bool) -> SandboxLauncherMount {
    SandboxLauncherMount {
        kind: SandboxLauncherMountKind::Bind,
        source: Some(source.to_path_buf()),
        target: target.to_path_buf(),
        readonly,
        options: Vec::new(),
    }
}

fn proc_mount(target: &Path) -> SandboxLauncherMount {
    SandboxLauncherMount {
        kind: SandboxLauncherMountKind::Proc,
        source: None,
        target: target.to_path_buf(),
        readonly: false,
        options: Vec::new(),
    }
}

fn tmpfs_mount(target: &Path, noexec: bool, extra_options: &[&str]) -> SandboxLauncherMount {
    let mut options = vec!["nosuid".to_string(), "nodev".to_string()];
    if noexec {
        options.push("noexec".to_string());
    }
    options.extend(extra_options.iter().map(|option| option.to_string()));
    SandboxLauncherMount {
        kind: SandboxLauncherMountKind::Tmpfs,
        source: None,
        target: target.to_path_buf(),
        readonly: false,
        options,
    }
}

fn effective_step_env(
    step: &SandboxRuntimeStep,
    build_seed_hex: &str,
    plain_object: bool,
) -> HashMap<String, String> {
    let mut env = HashMap::from([
        (
            // `/usr/bin` before `/usr/sbin` (and `/bin` before `/sbin`). The
            // rootfs is fully merged (`/usr/sbin -> bin`, `/sbin -> usr/bin`), so
            // both entries at each tier resolve to the same directory and the
            // order is functionally irrelevant to what gets executed. It matters
            // only for tools that record the PATH-resolved absolute path of an
            // interpreter: with sbin first, meson bakes `/usr/sbin/python3` into
            // installed script shebangs (e.g. g-ir-scanner); bin first yields the
            // canonical `/usr/bin/python3`, as in mainstream distros.
            "PATH".to_string(),
            "/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin".to_string(),
        ),
        ("HOME".to_string(), CONTAINER_BUILD_DIR.to_string()),
        ("TMPDIR".to_string(), "/tmp".to_string()),
        ("USER".to_string(), "bobr".to_string()),
        ("LC_ALL".to_string(), "C".to_string()),
        ("LANG".to_string(), "C".to_string()),
        ("TZ".to_string(), "UTC".to_string()),
        // 1980-01-01 UTC, not 0: tools like groff's mdate.pl treat
        // SOURCE_DATE_EPOCH=0 as unset (`$ENV{...} || mtime`; "0" is falsy in
        // Perl) and fall back to the build-time file mtime; pre-1980 dates also
        // break DOS-derived tools (zip). Matches nixpkgs.
        ("SOURCE_DATE_EPOCH".to_string(), "315532800".to_string()),
        ("PYTHONHASHSEED".to_string(), "0".to_string()),
        // Suppress import-time .pyc writes: CPython stamps them with the source
        // mtime, so any tool that imports a Python module during the build (e.g.
        // gdbus-codegen) can leave a non-reproducible __pycache__ entry in the
        // upper layer. Explicit compileall/py_compile still write bytecode.
        ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
        (
            "BOBR_CONFIG_DIR".to_string(),
            CONTAINER_CONFIG_DIR.to_string(),
        ),
        (
            "BOBR_BUILD_DIR".to_string(),
            CONTAINER_BUILD_DIR.to_string(),
        ),
        (
            "BOBR_INPUTS_DIR".to_string(),
            CONTAINER_INPUTS_DIR.to_string(),
        ),
        ("BOBR_STEP_NAME".to_string(), step.name.clone()),
        // Deterministic per-build seed (derived from the reuse key); builders
        // that need a reproducible "random" value read this.
        ("BOBR_BUILD_SEED".to_string(), build_seed_hex.to_string()),
    ]);
    // `$out` staging dir, only for the plain-object `Sandbox` path (it is the one
    // that binds `$out`). The additive path has no `$out`.
    if plain_object {
        env.insert("BOBR_OUT_DIR".to_string(), CONTAINER_OUT_DIR.to_string());
    }
    env.extend(step.env_overrides.clone());
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn runtime_step(temp: &tempfile::TempDir, name: &str) -> SandboxRuntimeStep {
        SandboxRuntimeStep {
            name: name.to_string(),
            run_as: StepUser::BuildUser,
            cwd: PathBuf::from("/"),
            argv: vec!["true".to_string()],
            env_overrides: HashMap::new(),
            umask: 0o022,
            stdout_path: temp.path().join(format!("{name}.stdout")),
            stderr_path: temp.path().join(format!("{name}.stderr")),
        }
    }

    // Returns the input plus the function-owned `workspace` that the runtime
    // function would create under `tmp`, so tests can drive the lower-level
    // helpers directly.
    fn valid_input(temp: &tempfile::TempDir) -> (SandboxInput, PathBuf) {
        let rootfs = temp.path().join("rootfs");
        let config_dir = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        for path in [&rootfs, &config_dir, &workspace] {
            fs::create_dir_all(path).unwrap();
        }
        let input = SandboxInput {
            rootfs,
            config_dir,
            tmp: temp.path().to_path_buf(),
            fs_tree: {
                let store_root = temp.path().join("store");
                fs::create_dir(&store_root).unwrap();
                bobr_store::Store::create(&store_root).unwrap().fs_tree()
            },
            launcher_path: temp.path().join(LAUNCHER_BINARY_NAME),
            extra_inputs: Vec::new(),
            steps: Vec::new(),
            build_seed_hex: String::new(),
            plain_object: false,
        };
        (input, workspace)
    }

    #[test]
    fn launcher_config_uses_root_overlay_and_writable_runtime_mounts() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("main.c"), "int main(void) { return 0; }\n").unwrap();
        let (mut input, workspace) = valid_input(&temp);
        for name in ["dev", "etc", "proc", "run", "tmp", "usr", "var"] {
            fs::create_dir(input.rootfs.join(name)).unwrap();
        }
        symlink("usr/bin", input.rootfs.join("bin")).unwrap();
        input.extra_inputs.push(crate::SandboxRuntimeInput {
            name: "source".to_string(),
            path: source.clone(),
        });
        let dirs = SandboxDirs::create(&workspace).unwrap();
        let runtime_files = SandboxRuntimeFiles::create(&dirs.runtime_files, &input).unwrap();
        fs::write(&input.launcher_path, "#!/bin/sh\n").unwrap();

        let launcher = build_launcher_config(&input, &dirs, &runtime_files, None).unwrap();
        let mounts = &launcher.mounts;

        assert_eq!(launcher.protocol_version, SANDBOX_PROTOCOL_VERSION);
        assert_eq!(launcher.root, dirs.rootfs);
        let overlay = launcher
            .root_overlay
            .as_ref()
            .expect("root is established via an overlay");
        assert_eq!(overlay.lower, input.rootfs);
        assert_eq!(overlay.upper, dirs.upper);
        assert_eq!(overlay.work, dirs.work);
        // The rootfs is no longer bound entry by entry; the overlay provides it.
        for name in ["usr", "etc", "var", "dev"] {
            assert!(!mounts.iter().any(|mount| {
                mount.source.as_deref() == Some(input.rootfs.join(name).as_path())
            }));
        }
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new(CONTAINER_BUILD_DIR)
                && mount.source.as_deref() == Some(dirs.build_dir.as_path())
                && !mount.readonly
        }));
        let source_bind = mounts
            .iter()
            .find(|mount| mount.target == Path::new(CONTAINER_INPUTS_DIR).join("source"))
            .expect("source input bind mount exists");
        assert_eq!(source_bind.source.as_deref(), Some(source.as_path()));
        assert!(source_bind.readonly);
        assert!(mounts.iter().any(|mount| {
            mount.target
                == Path::new(CONTAINER_LAUNCHER_DIR)
                    .join(LAUNCHER_BINARY_NAME)
                    .as_path()
                && mount.source.as_deref() == Some(input.launcher_path.as_path())
                && mount.readonly
        }));
    }

    #[test]
    fn launcher_config_rejects_duplicate_mount_targets() {
        let mounts = vec![
            tmpfs_mount(Path::new("/tmp"), false, &[]),
            bind_mount(Path::new("/dev/null"), Path::new("/tmp"), true),
        ];
        let config = SandboxLauncherConfig {
            protocol_version: SANDBOX_PROTOCOL_VERSION,
            root: PathBuf::from("/tmp/root"),
            mounts,
            runner_config: PathBuf::from(CONTAINER_RUNNER_CONFIG),
            failure_report: PathBuf::from("/tmp/failure.json"),
            root_overlay: None,
        };

        let error = validate_launcher_config(&config).unwrap_err();

        assert!(error.to_string().contains("defined more than once"));
    }

    #[test]
    fn overlay_upper_seeds_dev_symlinks_only() {
        let temp = tempdir().unwrap();
        let upper = temp.path().join("upper");
        fs::create_dir_all(&upper).unwrap();

        populate_overlay_upper(&upper).unwrap();

        // The /dev convenience symlinks are the only thing seeded into upper.
        assert_eq!(
            fs::read_link(upper.join("dev/fd")).unwrap(),
            Path::new("/proc/self/fd")
        );
        assert_eq!(
            fs::read_link(upper.join("dev/stdin")).unwrap(),
            Path::new("/proc/self/fd/0")
        );
        assert_eq!(
            fs::read_link(upper.join("dev/stdout")).unwrap(),
            Path::new("/proc/self/fd/1")
        );
        assert_eq!(
            fs::read_link(upper.join("dev/stderr")).unwrap(),
            Path::new("/proc/self/fd/2")
        );
        // No top-level rootfs entries are recreated; the lower layer provides them.
        assert!(!upper.join("usr").exists());
        assert!(!upper.join("bin").is_symlink());
    }

    #[test]
    fn effective_step_env_includes_reproducible_defaults_and_overrides() {
        let temp = tempdir().unwrap();
        let mut step = runtime_step(&temp, "build");
        step.env_overrides
            .insert("USER".to_string(), "custom".to_string());
        step.env_overrides
            .insert("SOURCE_DATE_EPOCH".to_string(), "123".to_string());

        let env = effective_step_env(&step, "abcd1234", false);

        assert_eq!(env["BOBR_STEP_NAME"], "build");
        assert_eq!(env["LC_ALL"], "C");
        assert_eq!(env["LANG"], "C");
        assert_eq!(env["TZ"], "UTC");
        assert_eq!(env["PYTHONHASHSEED"], "0");
        assert_eq!(env["PYTHONDONTWRITEBYTECODE"], "1");
        assert_eq!(env["USER"], "custom");
        assert_eq!(env["SOURCE_DATE_EPOCH"], "123");
        assert_eq!(env["BOBR_BUILD_SEED"], "abcd1234");
    }

    #[test]
    fn runner_config_serializes_effective_env_and_umask() {
        let temp = tempdir().unwrap();
        let (mut input, workspace) = valid_input(&temp);
        let mut step = runtime_step(&temp, "build");
        step.env_overrides
            .insert("SOURCE_DATE_EPOCH".to_string(), "123".to_string());
        step.umask = 0o077;
        input.steps.push(step);
        let runtime_files =
            SandboxRuntimeFiles::create(&workspace.join("runtime-files"), &input).unwrap();

        write_runner_config(&input, &runtime_files).unwrap();

        let runner_config: RunnerConfig =
            serde_json::from_slice(&fs::read(&runtime_files.runner_config).unwrap()).unwrap();
        assert_eq!(runner_config.steps[0].umask, 0o077);
        assert_eq!(runner_config.steps[0].env["SOURCE_DATE_EPOCH"], "123");
        assert_eq!(runner_config.steps[0].env["LC_ALL"], "C");
    }
}
