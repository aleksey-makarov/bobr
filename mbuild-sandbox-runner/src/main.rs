use mbuild_sandbox_runner_core::{
    RUNNER_PROTOCOL_VERSION, SandboxLauncherConfig, SandboxLauncherMount, SandboxLauncherMountKind,
    SandboxRunnerFailureReport, protocol_info, relative_launcher_target, run_config_path,
    validate_launcher_config,
};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const CAP_CHOWN: u32 = 0;
const CAP_DAC_OVERRIDE: u32 = 1;
const CAP_DAC_READ_SEARCH: u32 = 2;
const CAP_FOWNER: u32 = 3;
const CAP_FSETID: u32 = 4;
const CAP_KILL: u32 = 5;
const CAP_SETGID: u32 = 6;
const CAP_SETUID: u32 = 7;
const CAP_SETPCAP: u32 = 8;

const KEEP_CAPABILITIES: &[u32] = &[
    CAP_CHOWN,
    CAP_DAC_OVERRIDE,
    CAP_DAC_READ_SEARCH,
    CAP_FOWNER,
    CAP_FSETID,
    CAP_KILL,
    CAP_SETGID,
    CAP_SETUID,
    CAP_SETPCAP,
];
const LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;

#[repr(C)]
#[derive(Clone, Copy)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() == 2 && args[1] == "--protocol-info" {
        match serde_json::to_string(&protocol_info()) {
            Ok(json) => {
                println!("{json}");
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("failed to serialize protocol info: {error}");
                std::process::exit(2);
            }
        }
    }

    match parse_launch_args(&args) {
        Ok((wait_fd, config_path)) => {
            std::process::exit(launch(wait_fd, &config_path));
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

fn parse_launch_args(args: &[String]) -> Result<(RawFd, PathBuf), String> {
    if args.len() != 6 || args[1] != "launch" || args[2] != "--wait-fd" || args[4] != "--config" {
        return Err("usage: mbuild-sandbox-runner launch --wait-fd FD --config PATH".to_string());
    }
    let wait_fd = args[3]
        .parse::<RawFd>()
        .map_err(|error| format!("invalid --wait-fd '{}': {error}", args[3]))?;
    Ok((wait_fd, PathBuf::from(&args[5])))
}

fn launch(wait_fd: RawFd, config_path: &Path) -> i32 {
    let config = match read_launcher_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };

    if let Err(error) = read_one_byte(wait_fd) {
        write_launcher_failure(&config.failure_report, "launcher-handshake", error);
        return 1;
    }

    match run_supervisor(&config) {
        Ok(code) => code,
        Err(error) => {
            write_launcher_failure(&config.failure_report, "launcher-bootstrap", error);
            1
        }
    }
}

fn read_launcher_config(path: &Path) -> Result<SandboxLauncherConfig, String> {
    let bytes = fs::read(path).map_err(|error| {
        format!(
            "failed to read launcher config '{}': {error}",
            path.display()
        )
    })?;
    let config = serde_json::from_slice::<SandboxLauncherConfig>(&bytes).map_err(|error| {
        format!(
            "failed to parse launcher config '{}': {error}",
            path.display()
        )
    })?;
    if config.protocol_version != RUNNER_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported launcher protocol {}; expected {}",
            config.protocol_version, RUNNER_PROTOCOL_VERSION
        ));
    }
    validate_launcher_config(&config)
        .map_err(|error| format!("invalid launcher config '{}': {error}", path.display()))?;
    Ok(config)
}

fn run_supervisor(config: &SandboxLauncherConfig) -> io::Result<i32> {
    unshare_namespace("mount namespace", libc::CLONE_NEWNS)?;
    unshare_namespace("network namespace", libc::CLONE_NEWNET)?;
    unshare_namespace("UTS namespace", libc::CLONE_NEWUTS)?;
    unshare_namespace("IPC namespace", libc::CLONE_NEWIPC)?;
    set_hostname("mbuild").map_err(|error| context_error("set hostname", error))?;
    mount_private().map_err(|error| context_error("make mount tree private", error))?;
    unshare_namespace("PID namespace", libc::CLONE_NEWPID)?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let code = match run_pid1(config) {
            Ok(code) => code,
            Err(error) => {
                write_launcher_failure(&config.failure_report, "launcher-pid1", error);
                1
            }
        };
        unsafe { libc::_exit(code) };
    }

    wait_for_child(pid)
}

fn run_pid1(config: &SandboxLauncherConfig) -> io::Result<i32> {
    mount_layout(config).map_err(|error| context_error("mount sandbox layout", error))?;
    set_no_new_privs().map_err(|error| context_error("set no_new_privs", error))?;
    chroot(&config.root)
        .map_err(|error| context_error(format!("chroot '{}'", config.root.display()), error))?;
    std::env::set_current_dir("/")
        .map_err(|error| context_error("chdir '/' after chroot", error))?;
    drop_capabilities().map_err(|error| context_error("drop capabilities", error))?;
    Ok(run_config_path(&config.runner_config))
}

fn mount_layout(config: &SandboxLauncherConfig) -> io::Result<()> {
    for mount in &config.mounts {
        prepare_mount_target(&config.root, mount).map_err(|error| {
            context_error(
                format!("prepare mount target '{}'", mount.target.display()),
                error,
            )
        })?;
        match mount.kind {
            SandboxLauncherMountKind::Bind => bind_mount(&config.root, mount)?,
            SandboxLauncherMountKind::Proc => proc_mount(&config.root, mount)?,
            SandboxLauncherMountKind::Tmpfs => tmpfs_mount(&config.root, mount)?,
        }
    }
    Ok(())
}

fn prepare_mount_target(root: &Path, mount: &SandboxLauncherMount) -> io::Result<()> {
    let target = root.join(relative_launcher_target(&mount.target)?);
    match mount.kind {
        SandboxLauncherMountKind::Bind => {
            let source = mount.source.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "bind source missing")
            })?;
            let metadata = fs::metadata(source)?;
            if metadata.is_dir() {
                fs::create_dir_all(&target)
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                File::create(&target).map(|_| ())
            }
        }
        SandboxLauncherMountKind::Proc | SandboxLauncherMountKind::Tmpfs => {
            fs::create_dir_all(&target)
        }
    }
}

fn bind_mount(root: &Path, mount: &SandboxLauncherMount) -> io::Result<()> {
    let source = mount
        .source
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bind source missing"))?;
    let target = root.join(relative_launcher_target(&mount.target)?);
    let metadata = fs::metadata(source)?;
    let bind_flags = if metadata.is_dir() {
        libc::MS_BIND | libc::MS_REC
    } else {
        libc::MS_BIND
    };
    mount_syscall(Some(source), &target, None, bind_flags, None).map_err(|error| {
        context_error(
            format!(
                "bind mount '{}' -> '{}'",
                source.display(),
                mount.target.display()
            ),
            error,
        )
    })?;
    if mount.readonly {
        mount_syscall(
            Some(source),
            &target,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            None,
        )
        .map_err(|error| {
            context_error(
                format!("remount readonly bind '{}'", mount.target.display()),
                error,
            )
        })?;
    }
    Ok(())
}

fn proc_mount(root: &Path, mount: &SandboxLauncherMount) -> io::Result<()> {
    let target = root.join(relative_launcher_target(&mount.target)?);
    mount_syscall(Some(Path::new("proc")), &target, Some("proc"), 0, None)
        .map_err(|error| context_error(format!("mount proc '{}'", mount.target.display()), error))
}

fn tmpfs_mount(root: &Path, mount: &SandboxLauncherMount) -> io::Result<()> {
    let target = root.join(relative_launcher_target(&mount.target)?);
    let mut flags = 0;
    let mut data_options = Vec::new();
    for option in &mount.options {
        match option.as_str() {
            "nosuid" => flags |= libc::MS_NOSUID,
            "nodev" => flags |= libc::MS_NODEV,
            "noexec" => flags |= libc::MS_NOEXEC,
            option => data_options.push(option),
        }
    }
    let data = data_options.join(",");
    let data = if data.is_empty() {
        None
    } else {
        Some(data.as_str())
    };
    mount_syscall(
        Some(Path::new("tmpfs")),
        &target,
        Some("tmpfs"),
        flags,
        data,
    )
    .map_err(|error| context_error(format!("mount tmpfs '{}'", mount.target.display()), error))
}

fn mount_private() -> io::Result<()> {
    mount_syscall(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
}

fn mount_syscall(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let c_source = source.map(path_cstring).transpose()?;
    let c_target = path_cstring(target)?;
    let c_fstype = fstype.map(CString::new).transpose()?;
    let c_data = data.map(CString::new).transpose()?;
    let result = unsafe {
        libc::mount(
            c_source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            c_target.as_ptr(),
            c_fstype
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            c_data
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn chroot(root: &Path) -> io::Result<()> {
    let c_root = path_cstring(root)?;
    let result = unsafe { libc::chroot(c_root.as_ptr()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unshare_namespace(label: &str, flags: libc::c_int) -> io::Result<()> {
    if unsafe { libc::unshare(flags) } == 0 {
        Ok(())
    } else {
        Err(context_error(
            format!("unshare {label}"),
            io::Error::last_os_error(),
        ))
    }
}

fn context_error(context: impl std::fmt::Display, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

fn set_hostname(name: &str) -> io::Result<()> {
    let bytes = name.as_bytes();
    if unsafe { libc::sethostname(bytes.as_ptr().cast(), bytes.len()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_no_new_privs() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn drop_capabilities() -> io::Result<()> {
    let mask = capability_mask(KEEP_CAPABILITIES);

    for cap in 0_u32..64 {
        if !KEEP_CAPABILITIES.contains(&cap) {
            unsafe {
                libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
            }
        }
    }

    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [
        CapData {
            effective: mask[0],
            permitted: mask[0],
            inheritable: mask[0],
        },
        CapData {
            effective: mask[1],
            permitted: mask[1],
            inheritable: mask[1],
        },
    ];
    let result = unsafe {
        libc::syscall(
            libc::SYS_capset,
            (&mut header as *mut CapHeader).cast::<libc::c_void>(),
            data.as_ptr().cast::<libc::c_void>(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn capability_mask(capabilities: &[u32]) -> [u32; 2] {
    let mut mask = [0_u32; 2];
    for cap in capabilities {
        let index = (cap / 32) as usize;
        let bit = cap % 32;
        mask[index] |= 1_u32 << bit;
    }
    mask
}

fn wait_for_child(pid: libc::pid_t) -> io::Result<i32> {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            if libc::WIFEXITED(status) {
                return Ok(libc::WEXITSTATUS(status));
            }
            if libc::WIFSIGNALED(status) {
                return Ok(128 + libc::WTERMSIG(status));
            }
            return Ok(1);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_one_byte(fd: RawFd) -> io::Result<()> {
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte)
}

fn write_launcher_failure(path: &Path, label: &str, error: impl std::fmt::Display) {
    let report = SandboxRunnerFailureReport::runtime(label, error.to_string());
    if let Ok(file) = File::create(path) {
        let _ = serde_json::to_writer(file, &report);
    }
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains NUL byte: '{}'", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_launch_args() {
        let args = vec![
            "mbuild-sandbox-runner".to_string(),
            "launch".to_string(),
            "--wait-fd".to_string(),
            "7".to_string(),
            "--config".to_string(),
            "/tmp/config.json".to_string(),
        ];

        let (fd, path) = parse_launch_args(&args).unwrap();

        assert_eq!(fd, 7);
        assert_eq!(path, PathBuf::from("/tmp/config.json"));
    }

    #[test]
    fn rejects_old_bare_runner_config_mode() {
        let args = vec![
            "mbuild-sandbox-runner".to_string(),
            "/tmp/runner-config.json".to_string(),
        ];

        assert!(parse_launch_args(&args).unwrap_err().contains("launch"));
    }

    #[test]
    fn kept_capabilities_include_kill() {
        assert!(KEEP_CAPABILITIES.contains(&CAP_KILL));

        let mask = capability_mask(KEEP_CAPABILITIES);
        assert_ne!(mask[0] & (1_u32 << CAP_KILL), 0);
    }

    #[test]
    fn relative_target_rejects_unsafe_paths() {
        for target in ["relative", "/", "/tmp/../out", "/tmp/./out"] {
            assert!(
                relative_launcher_target(Path::new(target)).is_err(),
                "{target} should be rejected"
            );
        }
        assert_eq!(
            relative_launcher_target(Path::new("/__mbuild/out")).unwrap(),
            PathBuf::from("__mbuild/out")
        );
    }

    #[test]
    fn launcher_config_validation_rejects_duplicate_mount_targets() {
        let config = SandboxLauncherConfig {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            root: PathBuf::from("/tmp/root"),
            mounts: vec![
                SandboxLauncherMount {
                    kind: SandboxLauncherMountKind::Tmpfs,
                    source: None,
                    target: PathBuf::from("/tmp"),
                    readonly: false,
                    options: Vec::new(),
                },
                SandboxLauncherMount {
                    kind: SandboxLauncherMountKind::Tmpfs,
                    source: None,
                    target: PathBuf::from("/tmp"),
                    readonly: false,
                    options: Vec::new(),
                },
            ],
            runner_config: PathBuf::from("/__mbuild/runtime/runner-config.json"),
            failure_report: PathBuf::from("/tmp/failure.json"),
        };

        let error = validate_launcher_config(&config).unwrap_err();

        assert!(error.to_string().contains("defined more than once"));
    }
}
