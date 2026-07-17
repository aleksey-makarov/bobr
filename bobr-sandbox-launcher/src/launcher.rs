//! Privileged launcher: sets up namespaces, mounts, capabilities, and chroot,
//! then runs the in-namespace runner as pid 1.

use crate::protocol::{
    SANDBOX_PROTOCOL_VERSION, SandboxLauncherConfig, SandboxLauncherMount,
    SandboxLauncherMountKind, SandboxRunnerFailureReport, read_handshake_byte,
    relative_launcher_target, validate_launcher_config,
};
use crate::runner::{RunnerOutcome, run_config_path};
use nix::errno::Errno;
use nix::mount::MsFlags;
use nix::sched::CloneFlags;
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;
use std::fs::{self, File};
use std::io;
use std::os::fd::RawFd;
use std::path::Path;

/// Maps a `nix` errno into an `io::Error` that preserves the OS error code (and
/// thus its `io::ErrorKind`), unlike `io::Error::other`.
fn nix_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

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

/// Reads the launcher config, waits for the parent handshake, and runs the
/// supervisor. Returns the process exit code.
pub fn launch(wait_fd: RawFd, config_path: &Path) -> i32 {
    let config = match read_launcher_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let launcher_failure_report = match File::create(&config.failure_report) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "failed to open launcher failure report '{}': {error}",
                config.failure_report.display()
            );
            return 2;
        }
    };

    if let Err(error) = read_handshake_byte(wait_fd) {
        write_launcher_failure(&launcher_failure_report, "launcher-handshake", error);
        return 1;
    }
    // The handshake pipe is no longer needed; close it so it is not inherited
    // by the runner or any build step.
    unsafe {
        libc::close(wait_fd);
    }

    match run_supervisor(&config, &launcher_failure_report) {
        Ok(code) => code,
        Err(error) => {
            write_launcher_failure(&launcher_failure_report, "launcher-bootstrap", error);
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
    if config.protocol_version != SANDBOX_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported launcher protocol {}; expected {}",
            config.protocol_version, SANDBOX_PROTOCOL_VERSION
        ));
    }
    validate_launcher_config(&config)
        .map_err(|error| format!("invalid launcher config '{}': {error}", path.display()))?;
    Ok(config)
}

fn run_supervisor(config: &SandboxLauncherConfig, failure_report: &File) -> io::Result<i32> {
    unshare_namespace("mount namespace", CloneFlags::CLONE_NEWNS)?;
    unshare_namespace("network namespace", CloneFlags::CLONE_NEWNET)?;
    bring_loopback_up().map_err(|error| context_error("bring up loopback", error))?;
    unshare_namespace("UTS namespace", CloneFlags::CLONE_NEWUTS)?;
    unshare_namespace("IPC namespace", CloneFlags::CLONE_NEWIPC)?;
    set_hostname("bobr").map_err(|error| context_error("set hostname", error))?;
    mount_private().map_err(|error| context_error("make mount tree private", error))?;
    unshare_namespace("PID namespace", CloneFlags::CLONE_NEWPID)?;

    // INVARIANT: the launcher must stay single-threaded up to this fork. The
    // child below allocates, does file I/O and serde — operations that are only
    // safe in a forked child because no other thread exists. Spawning any thread
    // before this point would turn that into silent undefined behavior.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Tie the sandbox lifetime to the supervisor: if it dies, the kernel
        // SIGKILLs pid1, and a dying namespace init tears down every step.
        // SIGKILL because pid1 (the namespace init) ignores unhandled signals
        // like SIGTERM. getppid() can't detect an already-dead parent here
        // (pid1's parent lives outside this namespace), so the tiny
        // fork->prctl race is accepted; best effort on failure.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
        }
        let code = match run_pid1(config, failure_report) {
            Ok(code) => code,
            Err(error) => {
                write_launcher_failure(failure_report, "launcher-pid1", error);
                1
            }
        };
        unsafe { libc::_exit(code) };
    }

    wait_for_child(Pid::from_raw(pid))
}

fn run_pid1(config: &SandboxLauncherConfig, failure_report: &File) -> io::Result<i32> {
    mount_root_overlay(config).map_err(|error| context_error("mount root overlay", error))?;
    mount_layout(config).map_err(|error| context_error("mount sandbox layout", error))?;
    set_no_new_privs().map_err(|error| context_error("set no_new_privs", error))?;
    chroot(&config.root)
        .map_err(|error| context_error(format!("chroot '{}'", config.root.display()), error))?;
    std::env::set_current_dir("/")
        .map_err(|error| context_error("chdir '/' after chroot", error))?;
    drop_capabilities().map_err(|error| context_error("drop capabilities", error))?;
    match run_config_path(&config.runner_config) {
        // The runner created its reports and owns their contents; the launcher
        // must not touch the failure report on this path.
        RunnerOutcome::Reported(code) => Ok(code),
        // The runner failed before owning its reports, so it wrote nothing.
        // Record the structured report through the launcher's pre-opened fd
        // (the same on-disk file the runner would have written).
        RunnerOutcome::EarlyFailure(report) => {
            write_launcher_report(failure_report, &report);
            Ok(2)
        }
    }
}

fn mount_layout(config: &SandboxLauncherConfig) -> io::Result<()> {
    for mount in &config.mounts {
        // Resolve the in-root target once per mount; the kind helpers reuse it.
        let target = config
            .root
            .join(relative_launcher_target(&mount.target).map_err(|error| {
                context_error(
                    format!("resolve mount target '{}'", mount.target.display()),
                    error,
                )
            })?);
        match mount.kind {
            SandboxLauncherMountKind::Bind => bind_mount(mount, &target)?,
            SandboxLauncherMountKind::Proc => proc_mount(mount, &target)?,
            SandboxLauncherMountKind::Tmpfs => tmpfs_mount(mount, &target)?,
        }
    }
    Ok(())
}

fn prepare_bind_target(target: &Path, source_is_dir: bool) -> io::Result<()> {
    if source_is_dir {
        fs::create_dir_all(target)
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        File::create(target).map(|_| ())
    }
}

fn bind_mount(mount: &SandboxLauncherMount, target: &Path) -> io::Result<()> {
    let source = mount
        .source
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bind source missing"))?;
    // Stat the source once: it decides both the target shape and the bind flags.
    let source_is_dir = fs::metadata(source)
        .map_err(|error| context_error(format!("stat bind source '{}'", source.display()), error))?
        .is_dir();
    prepare_bind_target(target, source_is_dir).map_err(|error| {
        context_error(
            format!("prepare mount target '{}'", mount.target.display()),
            error,
        )
    })?;
    let bind_flags = if source_is_dir {
        MsFlags::MS_BIND | MsFlags::MS_REC
    } else {
        MsFlags::MS_BIND
    };
    do_mount(Some(source), target, None, bind_flags, None).map_err(|error| {
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
        do_mount(
            Some(source),
            target,
            None,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
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

fn proc_mount(mount: &SandboxLauncherMount, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target).map_err(|error| {
        context_error(
            format!("prepare mount target '{}'", mount.target.display()),
            error,
        )
    })?;
    let mut flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    if mount.readonly {
        flags |= MsFlags::MS_RDONLY;
    }
    do_mount(Some(Path::new("proc")), target, Some("proc"), flags, None)
        .map_err(|error| context_error(format!("mount proc '{}'", mount.target.display()), error))
}

fn tmpfs_mount(mount: &SandboxLauncherMount, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target).map_err(|error| {
        context_error(
            format!("prepare mount target '{}'", mount.target.display()),
            error,
        )
    })?;
    let (flags, data) = tmpfs_mount_options(&mount.options, mount.readonly);
    do_mount(
        Some(Path::new("tmpfs")),
        target,
        Some("tmpfs"),
        flags,
        data.as_deref(),
    )
    .map_err(|error| context_error(format!("mount tmpfs '{}'", mount.target.display()), error))
}

/// Establishes the sandbox root as an overlay when `config.root_overlay` is
/// set, mounting it at `config.root` before the interior mounts. The upper and
/// work dirs must already exist on a filesystem that supports the required
/// xattrs; this runs unprivileged inside the sandbox user namespace. A no-op
/// when no root overlay is configured.
fn mount_root_overlay(config: &SandboxLauncherConfig) -> io::Result<()> {
    let Some(overlay) = &config.root_overlay else {
        return Ok(());
    };
    let data = format!(
        "lowerdir={},upperdir={},workdir={}",
        overlay.lower.display(),
        overlay.upper.display(),
        overlay.work.display()
    );
    do_mount(
        Some(Path::new("overlay")),
        &config.root,
        Some("overlay"),
        MsFlags::empty(),
        Some(&data),
    )
    .map_err(|error| {
        context_error(
            format!("mount root overlay at '{}'", config.root.display()),
            error,
        )
    })
}

/// Splits tmpfs mount options into kernel flag bits and the leftover `data`
/// string. Recognised security options become flags; everything else (e.g.
/// `size=`, `mode=`) is passed through as comma-joined mount data.
fn tmpfs_mount_options(options: &[String], readonly: bool) -> (MsFlags, Option<String>) {
    let mut flags = MsFlags::empty();
    let mut data_options = Vec::new();
    for option in options {
        match option.as_str() {
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            other => data_options.push(other),
        }
    }
    if readonly {
        flags |= MsFlags::MS_RDONLY;
    }
    let data = if data_options.is_empty() {
        None
    } else {
        Some(data_options.join(","))
    };
    (flags, data)
}

fn mount_private() -> io::Result<()> {
    do_mount(
        None,
        Path::new("/"),
        None,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None,
    )
}

fn do_mount(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: MsFlags,
    data: Option<&str>,
) -> io::Result<()> {
    nix::mount::mount(source, target, fstype, flags, data).map_err(nix_to_io)
}

fn chroot(root: &Path) -> io::Result<()> {
    nix::unistd::chroot(root).map_err(nix_to_io)
}

fn unshare_namespace(label: &str, flags: CloneFlags) -> io::Result<()> {
    nix::sched::unshare(flags)
        .map_err(|error| context_error(format!("unshare {label}"), nix_to_io(error)))
}

fn context_error(context: impl std::fmt::Display, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

fn set_hostname(name: &str) -> io::Result<()> {
    nix::unistd::sethostname(name).map_err(nix_to_io)
}

/// Brings the loopback interface up in the current network namespace.
///
/// A fresh netns has only `lo`, administratively down with no address, so even
/// `127.0.0.1` is unreachable. This does not grant any external connectivity:
/// the namespace has no other interface or route. It must run before
/// capabilities are dropped (it needs CAP_NET_ADMIN in the netns).
fn bring_loopback_up() -> io::Result<()> {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if sock < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = set_interface_up(sock, b"lo");
    unsafe {
        libc::close(sock);
    }
    result
}

fn set_interface_up(sock: libc::c_int, name: &[u8]) -> io::Result<()> {
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, &byte) in request.ifr_name.iter_mut().zip(name) {
        *slot = byte as libc::c_char;
    }
    if unsafe { libc::ioctl(sock, libc::SIOCGIFFLAGS as libc::Ioctl, &mut request) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
    }
    if unsafe { libc::ioctl(sock, libc::SIOCSIFFLAGS as libc::Ioctl, &mut request) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    // Inheritable is left empty: steps must not gain capabilities across
    // execve. (It is inert anyway without ambient or file capabilities.)
    let data = [
        CapData {
            effective: mask[0],
            permitted: mask[0],
            inheritable: 0,
        },
        CapData {
            effective: mask[1],
            permitted: mask[1],
            inheritable: 0,
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

fn wait_for_child(pid: Pid) -> io::Result<i32> {
    loop {
        match nix::sys::wait::waitpid(pid, None) {
            Ok(status) => return Ok(exit_code_from_wait_status(status)),
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(nix_to_io(error)),
        }
    }
}

/// Maps a `waitpid` result into a process exit code: the child's own code when
/// it exited, `128 + signal` when it was killed, and `1` for any other
/// (unexpected) status.
fn exit_code_from_wait_status(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, signal, _) => 128 + signal as i32,
        _ => 1,
    }
}

fn write_launcher_failure(file: &File, label: &str, error: impl std::fmt::Display) {
    let report = SandboxRunnerFailureReport::runtime(label, error.to_string());
    write_launcher_report(file, &report);
}

fn write_launcher_report(file: &File, report: &SandboxRunnerFailureReport) {
    let _ = serde_json::to_writer(file, report);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SandboxRootOverlay;

    #[test]
    fn kept_capabilities_include_kill() {
        assert!(KEEP_CAPABILITIES.contains(&CAP_KILL));

        let mask = capability_mask(KEEP_CAPABILITIES);
        assert_ne!(mask[0] & (1_u32 << CAP_KILL), 0);
    }

    #[test]
    fn launcher_failure_report_writes_through_open_file() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bobr-launcher-failure-{}-{suffix}.json",
            std::process::id()
        ));
        let file = File::create(&path).unwrap();

        write_launcher_failure(&file, "launcher-pid1", "drop capabilities: EPERM");
        drop(file);
        let report: SandboxRunnerFailureReport =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(report.label, "launcher-pid1");
        assert!(report.message.contains("drop capabilities"));
    }

    #[test]
    fn tmpfs_options_split_flags_and_data() {
        let (flags, data) = tmpfs_mount_options(
            &[
                "nosuid".to_string(),
                "nodev".to_string(),
                "noexec".to_string(),
                "size=64m".to_string(),
                "mode=1777".to_string(),
            ],
            false,
        );

        assert_eq!(
            flags,
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            "recognised options become flags, unrecognised ones do not"
        );
        assert_eq!(data.as_deref(), Some("size=64m,mode=1777"));
    }

    #[test]
    fn tmpfs_options_readonly_without_data() {
        let (flags, data) = tmpfs_mount_options(&[], true);

        assert_eq!(flags, MsFlags::MS_RDONLY);
        assert_eq!(data, None);
    }

    #[test]
    fn exit_code_maps_exit_and_signal() {
        assert_eq!(
            exit_code_from_wait_status(WaitStatus::Exited(Pid::from_raw(123), 7)),
            7
        );
        assert_eq!(
            exit_code_from_wait_status(WaitStatus::Signaled(
                Pid::from_raw(123),
                nix::sys::signal::Signal::SIGKILL,
                false
            )),
            128 + libc::SIGKILL
        );
    }

    #[test]
    fn launcher_writes_early_runner_failure_through_open_file() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bobr-early-failure-{}-{suffix}.json",
            std::process::id()
        ));
        let file = File::create(&path).unwrap();

        // A runner EarlyFailure report is forwarded verbatim through the
        // launcher's pre-opened failure-report fd.
        let report = SandboxRunnerFailureReport::runtime(
            "runner-config",
            "failed to parse runner config".to_string(),
        );
        write_launcher_report(&file, &report);
        drop(file);
        let written: SandboxRunnerFailureReport =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(written.label, "runner-config");
        assert!(written.message.contains("parse runner config"));
    }

    /// Real root overlay driven through [`mount_root_overlay`], inside a fresh
    /// user+mount namespace. Confirms the lower layer is visible through the
    /// mounted root and that a new write lands in `upperdir` (the layer we later
    /// capture). Forks first: `unshare(CLONE_NEWUSER)` requires a
    /// single-threaded process and the test harness is multithreaded. When
    /// unprivileged user namespaces are unavailable the child exits 42 and the
    /// test skips rather than fails.
    #[test]
    fn overlay_mount_captures_writes_in_upper() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("bobr-overlay-{}-{suffix}", std::process::id()));
        let lower = base.join("lower");
        let upper = base.join("upper");
        let work = base.join("work");
        let mnt = base.join("mnt");
        for dir in [&lower, &upper, &work, &mnt] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(lower.join("a.txt"), "hello\n").unwrap();

        let config = SandboxLauncherConfig {
            protocol_version: SANDBOX_PROTOCOL_VERSION,
            root: mnt.clone(),
            mounts: Vec::new(),
            runner_config: "/runner-config.json".into(),
            failure_report: "/failure.json".into(),
            root_overlay: Some(SandboxRootOverlay {
                lower: lower.clone(),
                upper: upper.clone(),
                work: work.clone(),
            }),
        };

        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();

        // SAFETY: the same fork primitive run_supervisor uses. The child only
        // makes syscalls and glibc-fork-safe allocations, then exits without
        // returning into the test harness.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child. Each failure maps to a distinct exit code; 42 means skip.
            let code = (|| -> i32 {
                if nix::sched::unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS).is_err()
                {
                    return 42;
                }
                if fs::write("/proc/self/setgroups", "deny").is_err()
                    || fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).is_err()
                    || fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).is_err()
                {
                    return 46;
                }
                if do_mount(
                    None,
                    Path::new("/"),
                    None,
                    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                    None,
                )
                .is_err()
                {
                    return 47;
                }
                if mount_root_overlay(&config).is_err() {
                    return 43;
                }
                if fs::read_to_string(mnt.join("a.txt")).ok().as_deref() != Some("hello\n") {
                    return 44;
                }
                if fs::write(mnt.join("b.txt"), "world\n").is_err()
                    || fs::read_to_string(upper.join("b.txt")).ok().as_deref() != Some("world\n")
                {
                    return 45;
                }
                0
            })();
            std::process::exit(code);
        }

        let status = nix::sys::wait::waitpid(Pid::from_raw(pid), None).unwrap();
        let _ = fs::remove_dir_all(&base);
        match status {
            WaitStatus::Exited(_, 0) => {}
            WaitStatus::Exited(_, 42) => eprintln!(
                "overlay_mount_captures_writes_in_upper: skipped \
                 (unprivileged user namespaces unavailable)"
            ),
            other => panic!(
                "overlay child failed: {other:?} (43 mount, 44 lower, 45 upper, 46 idmap, 47 private)"
            ),
        }
    }
}
