use super::config::{SandboxBuildConfig, SandboxRunAs, SandboxStep};
use super::{
    CONTAINER_BREADCRUMBS, CONTAINER_FAILURE_REPORT, CONTAINER_LOG_DIR, CONTAINER_RUNNER_CONFIG,
    CONTAINER_RUNNER_DIR, CONTAINER_RUNTIME_DIR, CONTAINER_SUCCESS_REPORT,
};
use crate::error::RuntimeError;
use mbuild_sandbox_runner_core::{
    RUNNER_BINARY_NAME, RUNNER_PROTOCOL_VERSION, RunnerConfig, RunnerRunAs, RunnerStepConfig,
    SandboxLauncherConfig, SandboxLauncherMount, SandboxLauncherMountKind,
    relative_launcher_target, validate_launcher_config,
};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) struct PreparedSandbox {
    pub(super) dirs: SandboxDirs,
    pub(super) runtime_files: SandboxRuntimeFiles,
    pub(super) launcher_config: PathBuf,
}

impl PreparedSandbox {
    pub(super) fn create(
        config: &SandboxBuildConfig,
        runner_path: &Path,
    ) -> Result<Self, RuntimeError> {
        let dirs = SandboxDirs::create(&config.workspace)?;
        let runtime_files = SandboxRuntimeFiles::create(&dirs.runtime_files, config)?;
        write_runner_config(config, &runtime_files)?;
        populate_root_skeleton(&dirs.rootfs, &config.root_dir, &runtime_files)?;
        let launcher_config = dirs.root.join("launcher-config.json");
        let launcher = build_launcher_config(config, &dirs, &runtime_files, runner_path)?;
        serde_json::to_writer(File::create(&launcher_config)?, &launcher)
            .map_err(|error| RuntimeError::Executor(error.to_string()))?;
        Ok(Self {
            dirs,
            runtime_files,
            launcher_config,
        })
    }
}

pub(super) struct SandboxDirs {
    pub(super) root: PathBuf,
    rootfs: PathBuf,
    build_dir: PathBuf,
    runtime_files: PathBuf,
}

impl SandboxDirs {
    fn create(workspace: &Path) -> Result<Self, RuntimeError> {
        let root = workspace
            .join("sandbox")
            .join(Uuid::new_v4().simple().to_string());
        let rootfs = root.join("rootfs");
        let build_dir = root.join("build");
        let runtime_files = root.join("runtime-files");

        fs::create_dir_all(&rootfs)?;
        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&runtime_files)?;

        Ok(Self {
            root,
            rootfs,
            build_dir,
            runtime_files,
        })
    }
}

fn write_runner_config(
    config: &SandboxBuildConfig,
    runtime_files: &SandboxRuntimeFiles,
) -> Result<(), RuntimeError> {
    let steps = config
        .steps
        .iter()
        .zip(runtime_files.step_logs.iter())
        .map(|(step, logs)| RunnerStepConfig {
            name: step.name.clone(),
            run_as: match step.run_as {
                SandboxRunAs::BuildUser => RunnerRunAs::BuildUser,
                SandboxRunAs::Root => RunnerRunAs::Root,
            },
            cwd: step.cwd.clone(),
            argv: step.argv.clone(),
            env: step_env(step),
            stdout_path: logs.container_stdout.clone(),
            stderr_path: logs.container_stderr.clone(),
            report_stdout_path: step.stdout_path.clone(),
            report_stderr_path: step.stderr_path.clone(),
        })
        .collect::<Vec<_>>();
    let runner_config = RunnerConfig {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        prepare_paths: vec![PathBuf::from("/__mbuild/build")],
        steps,
        output_dir: PathBuf::from("/__mbuild/out"),
        success_report: PathBuf::from(CONTAINER_SUCCESS_REPORT),
        failure_report: PathBuf::from(CONTAINER_FAILURE_REPORT),
        breadcrumbs: PathBuf::from(CONTAINER_BREADCRUMBS),
    };
    serde_json::to_writer(File::create(&runtime_files.runner_config)?, &runner_config)
        .map_err(|error| RuntimeError::Executor(error.to_string()))
}

pub(super) struct SandboxRuntimeFiles {
    root: PathBuf,
    pub(super) success_report: PathBuf,
    pub(super) failure_report: PathBuf,
    runner_config: PathBuf,
    step_logs: Vec<SandboxStepLogMounts>,
}

impl SandboxRuntimeFiles {
    fn create(root: &Path, config: &SandboxBuildConfig) -> Result<Self, RuntimeError> {
        fs::create_dir_all(root)?;
        let success_report = root.join("sandbox-success.json");
        let failure_report = root.join("sandbox-failure.json");
        let breadcrumbs = root.join("sandbox-breadcrumbs.log");
        let runner_config = root.join("runner-config.json");
        File::create(&success_report)?;
        File::create(&failure_report)?;
        File::create(&breadcrumbs)?;
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
    config: &SandboxBuildConfig,
    dirs: &SandboxDirs,
    runtime_files: &SandboxRuntimeFiles,
    runner_path: &Path,
) -> Result<SandboxLauncherConfig, RuntimeError> {
    let mut mounts = rootfs_top_level_mounts(&config.root_dir)?;
    mounts.extend([
        bind_mount(Path::new("/dev/null"), Path::new("/dev/null"), false),
        bind_mount(Path::new("/dev/zero"), Path::new("/dev/zero"), false),
        bind_mount(Path::new("/dev/full"), Path::new("/dev/full"), false),
        bind_mount(Path::new("/dev/random"), Path::new("/dev/random"), false),
        bind_mount(Path::new("/dev/urandom"), Path::new("/dev/urandom"), false),
        proc_mount(Path::new("/proc")),
        tmpfs_mount(Path::new("/tmp"), &["mode=1777"]),
        tmpfs_mount(Path::new("/run"), &["mode=755"]),
        bind_mount(&dirs.build_dir, Path::new("/__mbuild/build"), false),
        bind_mount(&config.config_dir, Path::new("/__mbuild/config"), true),
        bind_mount(&config.out_dir, Path::new("/__mbuild/out"), false),
        bind_mount(
            runner_path,
            &Path::new(CONTAINER_RUNNER_DIR).join(RUNNER_BINARY_NAME),
            true,
        ),
        bind_mount(&runtime_files.root, Path::new(CONTAINER_RUNTIME_DIR), false),
    ]);

    for log in &runtime_files.step_logs {
        mounts.push(bind_mount(&log.host_stdout, &log.container_stdout, false));
        mounts.push(bind_mount(&log.host_stderr, &log.container_stderr, false));
    }

    for input in &config.inputs {
        mounts.push(bind_mount(
            &input.host_path,
            &input_mount_path(&input.name),
            true,
        ));
    }
    let launcher = SandboxLauncherConfig {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        root: dirs.rootfs.clone(),
        mounts,
        runner_config: PathBuf::from(CONTAINER_RUNNER_CONFIG),
        failure_report: runtime_files.failure_report.clone(),
    };
    validate_launcher_config(&launcher)
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    Ok(launcher)
}

fn rootfs_top_level_mounts(rootfs: &Path) -> Result<Vec<SandboxLauncherMount>, RuntimeError> {
    let mut entries = rootfs_top_level_entries(rootfs)?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut mounts = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "sandbox rootfs '{}' contains non-UTF-8 top-level entry",
                rootfs.display()
            ))
        })?;
        let source = entry.path();
        let destination = Path::new("/").join(name);
        let file_type = entry.file_type()?;

        if !should_mount_rootfs_entry(name) {
            continue;
        }

        if file_type.is_dir() || file_type.is_file() {
            mounts.push(bind_mount(&source, &destination, true));
        } else if !file_type.is_symlink() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox rootfs '{}' top-level entry '/{name}' must be a file, directory, or symlink",
                rootfs.display()
            )));
        }
    }

    Ok(mounts)
}

fn rootfs_top_level_entries(rootfs: &Path) -> Result<Vec<fs::DirEntry>, RuntimeError> {
    fs::read_dir(rootfs)?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(RuntimeError::from)
}

fn should_mount_rootfs_entry(name: &str) -> bool {
    !matches!(name, "__mbuild" | "dev" | "proc" | "run" | "tmp")
}

fn populate_root_skeleton(
    sandbox_root: &Path,
    lower_rootfs: &Path,
    runtime_files: &SandboxRuntimeFiles,
) -> Result<(), RuntimeError> {
    for entry in rootfs_top_level_entries(lower_rootfs)? {
        let name = entry.file_name();
        let destination = sandbox_root.join(&name);
        let file_type = entry.file_type()?;

        if file_type.is_symlink() {
            if let Some(name) = name.to_str()
                && should_mount_rootfs_entry(name)
            {
                let target = fs::read_link(entry.path())?;
                if !destination.exists() && !destination.is_symlink() {
                    symlink(target, destination)?;
                }
            }
        } else if file_type.is_dir() {
            let name = name.to_str().ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "sandbox rootfs '{}' contains non-UTF-8 top-level entry",
                    lower_rootfs.display()
                ))
            })?;
            if !should_mount_rootfs_entry(name) {
                fs::create_dir_all(destination)?;
            }
        }
    }
    for path in [
        Path::new("__mbuild"),
        Path::new("__mbuild/build"),
        Path::new("__mbuild/config"),
        Path::new("__mbuild/inputs"),
        Path::new("__mbuild/logs"),
        Path::new("__mbuild/out"),
        Path::new("__mbuild/runner"),
        Path::new("__mbuild/runtime"),
        Path::new("dev"),
        Path::new("proc"),
        Path::new("run"),
        Path::new("tmp"),
    ] {
        fs::create_dir_all(sandbox_root.join(path))?;
    }
    create_dev_symlink(sandbox_root, "fd", "/proc/self/fd")?;
    create_dev_symlink(sandbox_root, "stdin", "/proc/self/fd/0")?;
    create_dev_symlink(sandbox_root, "stdout", "/proc/self/fd/1")?;
    create_dev_symlink(sandbox_root, "stderr", "/proc/self/fd/2")?;
    File::create(
        sandbox_root
            .join("__mbuild/runner")
            .join(RUNNER_BINARY_NAME),
    )?;
    for log in &runtime_files.step_logs {
        create_mount_target(sandbox_root, &log.container_stdout)?;
        create_mount_target(sandbox_root, &log.container_stderr)?;
    }
    Ok(())
}

fn create_dev_symlink(sandbox_root: &Path, name: &str, target: &str) -> Result<(), RuntimeError> {
    let link = sandbox_root.join("dev").join(name);
    if !link.exists() && !link.is_symlink() {
        symlink(target, link)?;
    }
    Ok(())
}

fn create_mount_target(sandbox_root: &Path, container_path: &Path) -> Result<(), RuntimeError> {
    let relative = relative_launcher_target(container_path)
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    let target = sandbox_root.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    File::create(target)?;
    Ok(())
}

fn input_mount_path(name: &str) -> PathBuf {
    Path::new("/__mbuild/inputs").join(name)
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

fn tmpfs_mount(target: &Path, extra_options: &[&str]) -> SandboxLauncherMount {
    let mut options = vec![
        "nosuid".to_string(),
        "nodev".to_string(),
        "noexec".to_string(),
    ];
    options.extend(extra_options.iter().map(|option| option.to_string()));
    SandboxLauncherMount {
        kind: SandboxLauncherMountKind::Tmpfs,
        source: None,
        target: target.to_path_buf(),
        readonly: false,
        options,
    }
}

fn step_env(step: &SandboxStep) -> HashMap<String, String> {
    let mut env = HashMap::from([
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        ("HOME".to_string(), "/__mbuild/build".to_string()),
        ("TMPDIR".to_string(), "/tmp".to_string()),
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

#[cfg(test)]
mod tests {
    use super::super::config::{SandboxInput, SandboxRunAs};
    use super::*;
    use tempfile::tempdir;

    fn valid_config(temp: &tempfile::TempDir) -> SandboxBuildConfig {
        let root_dir = temp.path().join("rootfs");
        let out_dir = temp.path().join("out");
        let config_dir = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        for path in [&root_dir, &out_dir, &config_dir, &workspace] {
            fs::create_dir_all(path).unwrap();
        }
        SandboxBuildConfig {
            root_dir,
            out_dir,
            config_dir,
            workspace,
            inputs: Vec::new(),
            steps: Vec::new(),
        }
    }

    #[test]
    fn launcher_config_uses_readonly_rootfs_binds_and_writable_runtime_mounts() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let source = temp.path().join("source");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        for path in [&rootfs, &source, &out, &config, &workspace] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(source.join("main.c"), "int main(void) { return 0; }\n").unwrap();
        for name in ["dev", "etc", "proc", "run", "tmp", "usr", "var"] {
            fs::create_dir(rootfs.join(name)).unwrap();
        }
        symlink("usr/bin", rootfs.join("bin")).unwrap();
        let build_config = SandboxBuildConfig {
            root_dir: rootfs.clone(),
            out_dir: out.clone(),
            config_dir: config,
            workspace: workspace.clone(),
            inputs: vec![SandboxInput {
                name: "source".to_string(),
                host_path: source.clone(),
            }],
            steps: Vec::new(),
        };
        let dirs = SandboxDirs::create(&workspace).unwrap();
        let runtime_files =
            SandboxRuntimeFiles::create(&dirs.runtime_files, &build_config).unwrap();
        let runner_path = temp.path().join(RUNNER_BINARY_NAME);
        fs::write(&runner_path, "#!/bin/sh\n").unwrap();

        let launcher =
            build_launcher_config(&build_config, &dirs, &runtime_files, &runner_path).unwrap();
        let mounts = &launcher.mounts;

        assert_eq!(launcher.protocol_version, RUNNER_PROTOCOL_VERSION);
        assert_eq!(launcher.root, dirs.rootfs);
        for name in ["usr", "etc", "var"] {
            let destination = Path::new("/").join(name);
            let mount = mounts
                .iter()
                .find(|mount| mount.target == destination)
                .unwrap_or_else(|| panic!("/{name} readonly bind mount exists"));
            assert_eq!(mount.kind, SandboxLauncherMountKind::Bind);
            assert_eq!(mount.source.as_deref(), Some(rootfs.join(name).as_path()));
            assert!(mount.readonly);
        }
        assert!(!mounts.iter().any(|mount| mount.target == Path::new("/dev")
            && mount.source.as_deref() == Some(rootfs.join("dev").as_path())));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/__mbuild/build")
                && mount.source.as_deref() == Some(dirs.build_dir.as_path())
                && !mount.readonly
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/__mbuild/out")
                && mount.source.as_deref() == Some(out.as_path())
                && !mount.readonly
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/tmp") && mount.kind == SandboxLauncherMountKind::Tmpfs
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/run") && mount.kind == SandboxLauncherMountKind::Tmpfs
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/__mbuild/config")
                && mount.source.as_deref() == Some(build_config.config_dir.as_path())
                && mount.readonly
        }));
        let source_bind = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/__mbuild/inputs/source"))
            .expect("source input bind mount exists");
        assert_eq!(source_bind.source.as_deref(), Some(source.as_path()));
        assert!(source_bind.readonly);
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.target == Path::new("/etc/hosts"))
        );
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.target == Path::new("/etc/resolv.conf"))
        );
        assert!(mounts.iter().any(|mount| {
            mount.target
                == Path::new(CONTAINER_RUNNER_DIR)
                    .join(RUNNER_BINARY_NAME)
                    .as_path()
                && mount.source.as_deref() == Some(runner_path.as_path())
                && mount.readonly
        }));
        assert!(mounts.iter().all(|mount| {
            mount
                .options
                .iter()
                .all(|option| !option.contains("cgroup"))
        }));
    }

    #[test]
    fn launcher_config_derives_input_mount_path() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        fs::create_dir(&input).unwrap();
        let mut build_config = valid_config(&temp);
        build_config.inputs.push(SandboxInput {
            name: "source".to_string(),
            host_path: input.clone(),
        });
        let dirs = SandboxDirs::create(&build_config.workspace).unwrap();
        let runtime_files =
            SandboxRuntimeFiles::create(&dirs.runtime_files, &build_config).unwrap();
        let runner_path = temp.path().join(RUNNER_BINARY_NAME);
        fs::write(&runner_path, "#!/bin/sh\n").unwrap();

        let launcher =
            build_launcher_config(&build_config, &dirs, &runtime_files, &runner_path).unwrap();

        let mount = launcher
            .mounts
            .iter()
            .find(|mount| mount.target == Path::new("/__mbuild/inputs/source"))
            .expect("derived input mount exists");
        assert_eq!(mount.source.as_deref(), Some(input.as_path()));
    }

    #[test]
    fn launcher_config_rejects_duplicate_mount_targets() {
        let mounts = vec![
            tmpfs_mount(Path::new("/tmp"), &[]),
            bind_mount(Path::new("/dev/null"), Path::new("/tmp"), true),
        ];
        let config = SandboxLauncherConfig {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            root: PathBuf::from("/tmp/root"),
            mounts,
            runner_config: PathBuf::from(CONTAINER_RUNNER_CONFIG),
            failure_report: PathBuf::from("/tmp/failure.json"),
        };

        let error = validate_launcher_config(&config).unwrap_err();

        assert!(error.to_string().contains("defined more than once"));
    }

    #[test]
    fn container_target_validation_rejects_unsafe_paths() {
        for path in ["relative", "/", "/tmp/../out", "/tmp/./out"] {
            let error = relative_launcher_target(Path::new(path))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("absolute")
                    || error.contains("must not be '/'")
                    || error.contains("must not contain")
            );
        }
    }

    #[test]
    fn sandbox_root_skeleton_copies_top_level_symlinks_and_skipped_dirs() {
        let temp = tempdir().unwrap();
        let lower = temp.path().join("lower");
        let root = temp.path().join("root");
        fs::create_dir_all(&lower).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(lower.join("run")).unwrap();
        fs::create_dir(lower.join("usr")).unwrap();
        symlink("usr/bin", lower.join("bin")).unwrap();

        let config = SandboxBuildConfig {
            root_dir: lower.clone(),
            out_dir: temp.path().join("out"),
            config_dir: temp.path().join("config"),
            workspace: temp.path().join("workspace"),
            inputs: Vec::new(),
            steps: Vec::new(),
        };
        for path in [&config.out_dir, &config.config_dir, &config.workspace] {
            fs::create_dir_all(path).unwrap();
        }
        let runtime_files =
            SandboxRuntimeFiles::create(&config.workspace.join("runtime-files"), &config).unwrap();

        populate_root_skeleton(&root, &lower, &runtime_files).unwrap();

        assert_eq!(
            fs::read_link(root.join("bin")).unwrap(),
            Path::new("usr/bin")
        );
        assert!(root.join("run").is_dir());
        assert!(!root.join("usr").exists());
        assert_eq!(
            fs::read_link(root.join("dev/fd")).unwrap(),
            Path::new("/proc/self/fd")
        );
        assert_eq!(
            fs::read_link(root.join("dev/stdin")).unwrap(),
            Path::new("/proc/self/fd/0")
        );
    }

    #[test]
    fn step_env_includes_defaults_and_overrides() {
        let step = SandboxStep {
            name: "build".to_string(),
            run_as: SandboxRunAs::BuildUser,
            cwd: PathBuf::from("/"),
            argv: vec!["true".to_string()],
            env: HashMap::from([("USER".to_string(), "custom".to_string())]),
            stdout_path: PathBuf::from("/tmp/stdout"),
            stderr_path: PathBuf::from("/tmp/stderr"),
        };

        let env = step_env(&step);

        assert_eq!(env["MBUILD_STEP_NAME"], "build");
        assert_eq!(env["USER"], "custom");
    }
}
