use mbuild_sandbox_runner_core::{
    RUNNER_PROTOCOL_VERSION, SandboxLauncherConfig, SandboxLauncherMount, SandboxLauncherMountKind,
    SandboxRunnerFailureReport, protocol_info, run_config_path,
};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const KEEP_CAPABILITIES: &[i32] = &[0, 1, 2, 3, 4, 6, 7, 8];
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
    Ok(config)
}

fn run_supervisor(config: &SandboxLauncherConfig) -> io::Result<i32> {
    unshare(libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC)?;
    set_hostname("mbuild")?;
    mount_private()?;
    unshare(libc::CLONE_NEWPID)?;

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
    mount_layout(config)?;
    set_no_new_privs()?;
    chroot(&config.root)?;
    std::env::set_current_dir("/")?;
    drop_capabilities()?;
    Ok(run_config_path(&config.runner_config))
}

fn mount_layout(config: &SandboxLauncherConfig) -> io::Result<()> {
    for mount in &config.mounts {
        prepare_mount_target(&config.root, mount)?;
        match mount.kind {
            SandboxLauncherMountKind::Bind => bind_mount(&config.root, mount)?,
            SandboxLauncherMountKind::Proc => proc_mount(&config.root, mount)?,
            SandboxLauncherMountKind::Tmpfs => tmpfs_mount(&config.root, mount)?,
        }
    }
    Ok(())
}

fn prepare_mount_target(root: &Path, mount: &SandboxLauncherMount) -> io::Result<()> {
    let target = root.join(relative_target(&mount.target)?);
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
    let target = root.join(relative_target(&mount.target)?);
    mount_syscall(
        Some(source),
        &target,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )?;
    if mount.readonly {
        mount_syscall(
            Some(source),
            &target,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC,
            None,
        )?;
    }
    Ok(())
}

fn proc_mount(root: &Path, mount: &SandboxLauncherMount) -> io::Result<()> {
    let target = root.join(relative_target(&mount.target)?);
    mount_syscall(None, &target, Some("proc"), 0, Some("proc"))
}

fn tmpfs_mount(root: &Path, mount: &SandboxLauncherMount) -> io::Result<()> {
    let target = root.join(relative_target(&mount.target)?);
    let data = mount.options.join(",");
    mount_syscall(None, &target, Some("tmpfs"), 0, Some(&data))
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

fn unshare(flags: libc::c_int) -> io::Result<()> {
    if unsafe { libc::unshare(flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
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
    let mut mask = [0_u32; 2];
    for cap in KEEP_CAPABILITIES {
        let index = (*cap as usize) / 32;
        let bit = (*cap as usize) % 32;
        mask[index] |= 1_u32 << bit;
    }

    for cap in 0..64 {
        if !KEEP_CAPABILITIES.contains(&cap) {
            unsafe {
                libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0);
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

fn relative_target(target: &Path) -> io::Result<&Path> {
    target.strip_prefix("/").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mount target '{}' must be absolute", target.display()),
        )
    })
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
}
