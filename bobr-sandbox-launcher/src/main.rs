//! Command-line entry point for `bobr-sandbox-launcher`: the privileged process
//! that sets up the sandbox (namespaces, mounts, chroot) and runs the build
//! steps. The protocol and implementation live in the library crate.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_sandbox_launcher::{launch, protocol_info};
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::path::PathBuf;

fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.len() == 2 && args[1].to_str() == Some("--protocol-info") {
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
        Ok((wait_fd, event_fd, config_path)) => {
            std::process::exit(launch(wait_fd, event_fd, &config_path));
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

fn parse_launch_args(args: &[OsString]) -> Result<(RawFd, RawFd, PathBuf), String> {
    let usage = || {
        "usage: bobr-sandbox-launcher launch --wait-fd FD --event-fd FD --config PATH".to_string()
    };
    if args.len() != 8
        || args[1].to_str() != Some("launch")
        || args[2].to_str() != Some("--wait-fd")
        || args[4].to_str() != Some("--event-fd")
        || args[6].to_str() != Some("--config")
    {
        return Err(usage());
    }
    let wait_fd = args[3]
        .to_str()
        .and_then(|value| value.parse::<RawFd>().ok())
        .ok_or_else(|| format!("invalid --wait-fd '{}'", args[3].to_string_lossy()))?;
    let event_fd = args[5]
        .to_str()
        .and_then(|value| value.parse::<RawFd>().ok())
        .ok_or_else(|| format!("invalid --event-fd '{}'", args[5].to_string_lossy()))?;
    if wait_fd <= libc::STDERR_FILENO || event_fd <= libc::STDERR_FILENO || wait_fd == event_fd {
        return Err("protocol fds must be distinct and must not be stdio".to_string());
    }
    Ok((wait_fd, event_fd, PathBuf::from(&args[7])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_launch_args() {
        let args = os_args(&[
            "bobr-sandbox-launcher",
            "launch",
            "--wait-fd",
            "7",
            "--event-fd",
            "8",
            "--config",
            "/tmp/config.json",
        ]);

        let (wait_fd, event_fd, path) = parse_launch_args(&args).unwrap();

        assert_eq!(wait_fd, 7);
        assert_eq!(event_fd, 8);
        assert_eq!(path, PathBuf::from("/tmp/config.json"));
    }

    #[test]
    fn rejects_old_bare_runner_config_mode() {
        let args = os_args(&["bobr-sandbox-launcher", "/tmp/runner-config.json"]);

        assert!(parse_launch_args(&args).unwrap_err().contains("launch"));
    }

    #[test]
    fn rejects_non_utf8_wait_fd_without_panicking() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![
            OsString::from("bobr-sandbox-launcher"),
            OsString::from("launch"),
            OsString::from("--wait-fd"),
            OsString::from_vec(vec![0xff, 0xfe]),
            OsString::from("--event-fd"),
            OsString::from("8"),
            OsString::from("--config"),
            OsString::from("/tmp/config.json"),
        ];

        assert!(parse_launch_args(&args).unwrap_err().contains("--wait-fd"));
    }

    #[test]
    fn rejects_stdio_and_duplicate_protocol_fds() {
        for (wait_fd, event_fd) in [(2, 8), (7, 2), (7, 7)] {
            let args = os_args(&[
                "bobr-sandbox-launcher",
                "launch",
                "--wait-fd",
                &wait_fd.to_string(),
                "--event-fd",
                &event_fd.to_string(),
                "--config",
                "/tmp/config.json",
            ]);

            assert!(parse_launch_args(&args).is_err());
        }
    }
}
