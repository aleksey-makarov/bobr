use mbuild_core::runtime_helper_protocol::{
    HELPER_BINARY_NAME, HELPER_PROTOCOL_VERSION, HelperProtocolInfo,
};
use std::env;
use std::ffi::OsString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
enum HelperCommand {
    ProtocolInfo,
    Ownership {
        config: PathBuf,
    },
    FsTreeTar {
        config: PathBuf,
    },
    FsTreeInitramfs {
        config: PathBuf,
    },
    FsTreeMaterialize {
        config: PathBuf,
    },
    WaitExec {
        wait_fd: RawFd,
        command: Vec<OsString>,
    },
}

pub(crate) fn main_from_env() -> ExitCode {
    match main_result(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn main_result(args: Vec<OsString>) -> Result<(), String> {
    match parse_args(args)? {
        HelperCommand::ProtocolInfo => {
            let info = HelperProtocolInfo {
                name: HELPER_BINARY_NAME.to_string(),
                protocol_version: HELPER_PROTOCOL_VERSION,
            };
            serde_json::to_writer(io::stdout(), &info)
                .map_err(|error| format!("failed to write protocol info: {error}"))?;
            Ok(())
        }
        HelperCommand::Ownership { config } => crate::ownership::run_config_path(&config),
        HelperCommand::FsTreeTar { config } => crate::tar_writer::run_config_path(&config),
        HelperCommand::FsTreeInitramfs { config } => {
            crate::initramfs_writer::run_config_path(&config)
        }
        HelperCommand::FsTreeMaterialize { config } => crate::materialize::run_config_path(&config),
        HelperCommand::WaitExec { wait_fd, command } => {
            wait_for_parent(wait_fd)?;
            exec_helper_command(command)
        }
    }
}

fn parse_args(args: Vec<OsString>) -> Result<HelperCommand, String> {
    if args.len() == 1 && args[0] == "--protocol-info" {
        return Ok(HelperCommand::ProtocolInfo);
    }

    if args.first().is_some_and(|arg| arg == "ownership") {
        let config = parse_config_args("ownership", &args[1..])?;
        return Ok(HelperCommand::Ownership { config });
    }

    if args.first().is_some_and(|arg| arg == "fs-tree-tar") {
        let config = parse_config_args("fs-tree-tar", &args[1..])?;
        return Ok(HelperCommand::FsTreeTar { config });
    }

    if args.first().is_some_and(|arg| arg == "fs-tree-initramfs") {
        let config = parse_config_args("fs-tree-initramfs", &args[1..])?;
        return Ok(HelperCommand::FsTreeInitramfs { config });
    }

    if args.first().is_some_and(|arg| arg == "fs-tree-materialize") {
        let config = parse_config_args("fs-tree-materialize", &args[1..])?;
        return Ok(HelperCommand::FsTreeMaterialize { config });
    }

    if args.first().is_some_and(|arg| arg == "wait-exec") {
        let (wait_fd, command) = parse_wait_exec_args(&args[1..])?;
        return Ok(HelperCommand::WaitExec { wait_fd, command });
    }

    Err(format!(
        "usage: {HELPER_BINARY_NAME} --protocol-info | ownership --config PATH | fs-tree-tar --config PATH | fs-tree-initramfs --config PATH | fs-tree-materialize --config PATH | wait-exec --wait-fd FD -- COMMAND [ARGS...]"
    ))
}

fn parse_config_args(command: &str, args: &[OsString]) -> Result<PathBuf, String> {
    let mut config = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--config") => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--config requires a path".to_string())?;
                config = Some(PathBuf::from(value));
            }
            Some(flag) => return Err(format!("unknown {command} argument '{flag}'")),
            None => return Err(format!("{command} arguments must be UTF-8")),
        }
        index += 1;
    }
    config.ok_or_else(|| format!("{command} requires --config"))
}

fn parse_wait_exec_args(args: &[OsString]) -> Result<(RawFd, Vec<OsString>), String> {
    let mut wait_fd = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--wait-fd") => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "--wait-fd requires a file descriptor".to_string())?;
                wait_fd = Some(
                    RawFd::from_str(value)
                        .map_err(|error| format!("invalid --wait-fd '{value}': {error}"))?,
                );
            }
            Some("--") => {
                let wait_fd = wait_fd.ok_or_else(|| "wait-exec requires --wait-fd".to_string())?;
                let command = args[index + 1..].to_vec();
                if command.is_empty() {
                    return Err("wait-exec requires a command after --".to_string());
                }
                return Ok((wait_fd, command));
            }
            Some(flag) => return Err(format!("unknown wait-exec argument '{flag}'")),
            None => return Err("wait-exec arguments must be UTF-8".to_string()),
        }
        index += 1;
    }

    if wait_fd.is_none() {
        return Err("wait-exec requires --wait-fd".to_string());
    }
    Err("wait-exec requires -- before command".to_string())
}

fn wait_for_parent(fd: RawFd) -> Result<(), String> {
    let mut byte = [0_u8; 1];
    loop {
        let result = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if result == 1 {
            return Ok(());
        }
        if result == 0 {
            return Err("parent closed setup pipe before signalling readiness".to_string());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("failed to read setup pipe: {error}"));
        }
    }
}

fn exec_helper_command(command: Vec<OsString>) -> Result<(), String> {
    let current_exe = env::current_exe()
        .map_err(|error| format!("failed to resolve runtime helper executable: {error}"))?;
    let error = Command::new(current_exe).args(command).exec();
    Err(format!("failed to exec runtime helper: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_protocol_info_command() {
        assert_eq!(
            parse_args(vec![OsString::from("--protocol-info")]).unwrap(),
            HelperCommand::ProtocolInfo
        );
    }

    #[test]
    fn parse_ownership_command() {
        assert_eq!(
            parse_args(vec![
                OsString::from("ownership"),
                OsString::from("--config"),
                OsString::from("/tmp/config.json"),
            ])
            .unwrap(),
            HelperCommand::Ownership {
                config: PathBuf::from("/tmp/config.json"),
            }
        );
    }

    #[test]
    fn parse_fs_tree_tar_command() {
        assert_eq!(
            parse_args(vec![
                OsString::from("fs-tree-tar"),
                OsString::from("--config"),
                OsString::from("/tmp/tar.json"),
            ])
            .unwrap(),
            HelperCommand::FsTreeTar {
                config: PathBuf::from("/tmp/tar.json"),
            }
        );
    }

    #[test]
    fn parse_fs_tree_initramfs_command() {
        assert_eq!(
            parse_args(vec![
                OsString::from("fs-tree-initramfs"),
                OsString::from("--config"),
                OsString::from("/tmp/initramfs.json"),
            ])
            .unwrap(),
            HelperCommand::FsTreeInitramfs {
                config: PathBuf::from("/tmp/initramfs.json"),
            }
        );
    }

    #[test]
    fn parse_fs_tree_materialize_command() {
        assert_eq!(
            parse_args(vec![
                OsString::from("fs-tree-materialize"),
                OsString::from("--config"),
                OsString::from("/tmp/materialize.json"),
            ])
            .unwrap(),
            HelperCommand::FsTreeMaterialize {
                config: PathBuf::from("/tmp/materialize.json"),
            }
        );
    }

    #[test]
    fn parse_wait_exec_command() {
        assert_eq!(
            parse_args(vec![
                OsString::from("wait-exec"),
                OsString::from("--wait-fd"),
                OsString::from("8"),
                OsString::from("--"),
                OsString::from("ownership"),
                OsString::from("--config"),
                OsString::from("/tmp/config.json"),
            ])
            .unwrap(),
            HelperCommand::WaitExec {
                wait_fd: 8,
                command: vec![
                    OsString::from("ownership"),
                    OsString::from("--config"),
                    OsString::from("/tmp/config.json"),
                ],
            }
        );
    }

    #[test]
    fn parse_wait_exec_rejects_missing_command_after_separator() {
        let error = parse_args(vec![
            OsString::from("wait-exec"),
            OsString::from("--wait-fd"),
            OsString::from("8"),
            OsString::from("--"),
        ])
        .unwrap_err();

        assert!(error.contains("requires a command after --"));
    }

    #[test]
    fn parse_ownership_rejects_wait_fd() {
        let error = parse_args(vec![
            OsString::from("ownership"),
            OsString::from("--config"),
            OsString::from("/tmp/config.json"),
            OsString::from("--wait-fd"),
            OsString::from("7"),
        ])
        .unwrap_err();

        assert!(error.contains("unknown ownership argument '--wait-fd'"));
    }

    #[test]
    fn parse_fs_tree_tar_requires_config() {
        let error = parse_args(vec![OsString::from("fs-tree-tar")]).unwrap_err();
        assert!(error.contains("--config"));
    }

    #[test]
    fn parse_ownership_requires_config() {
        let error = parse_args(vec![OsString::from("ownership")]).unwrap_err();
        assert!(error.contains("--config"));
    }

    #[test]
    fn parse_wait_exec_requires_wait_fd() {
        let error = parse_args(vec![
            OsString::from("wait-exec"),
            OsString::from("--"),
            OsString::from("ownership"),
            OsString::from("--config"),
            OsString::from("/tmp/config.json"),
        ])
        .unwrap_err();

        assert!(error.contains("requires --wait-fd"));
    }
}
