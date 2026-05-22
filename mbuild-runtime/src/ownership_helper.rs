//! Entry point and protocol for the local ownership helper binary.

use crate::executor::{
    ExecutorErrorReport, ExecutorResultTimings, write_executor_error_report,
    write_executor_result_report_with_timings,
};
use crate::idmap::MbuildIdmap;
use crate::ownership::{HashReport, OwnershipExecutor};
use mbuild_core::FsTreeManifest;
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::str::FromStr;

/// Helper binary name expected by the parent runtime.
pub const HELPER_BINARY_NAME: &str = "mbuild-runtime-helper";

/// Protocol version implemented by the ownership helper.
pub const HELPER_PROTOCOL_VERSION: u32 = 1;

/// Machine-readable protocol metadata printed by `--protocol-info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperProtocolInfo {
    /// Helper binary protocol name.
    pub name: String,
    /// Helper protocol version.
    pub protocol_version: u32,
}

/// Serializable id mapping passed to the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipHelperIdmap {
    /// Host uid mapped to logical uid 0.
    pub current_uid: u32,
    /// Host gid mapped to logical gid 0.
    pub current_gid: u32,
    /// First host uid mapped to positive logical uids.
    pub subuid_base: u32,
    /// Number of positive logical uids available.
    pub subuid_count: u32,
    /// First host gid mapped to positive logical gids.
    pub subgid_base: u32,
    /// Number of positive logical gids available.
    pub subgid_count: u32,
}

impl From<&MbuildIdmap> for OwnershipHelperIdmap {
    fn from(idmap: &MbuildIdmap) -> Self {
        Self {
            current_uid: idmap.current_uid(),
            current_gid: idmap.current_gid(),
            subuid_base: idmap.subuid_base(),
            subuid_count: idmap.subuid_count(),
            subgid_base: idmap.subgid_base(),
            subgid_count: idmap.subgid_count(),
        }
    }
}

/// Helper-side hash mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnershipHelperHashReport {
    /// Hash `target_root` directly after ownership materialization.
    TargetRoot,
    /// Hash a synthetic fs-tree object from a canonical manifest and root.
    FsTreeObject {
        /// Canonical `manifest.jsonl` bytes encoded as UTF-8 text.
        manifest: String,
        /// Additional top-level object files as `(name, content)` byte arrays.
        extra_files: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

/// JSON configuration consumed by the ownership helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipHelperConfig {
    /// Absolute host path to the target root.
    pub target_root: PathBuf,
    /// Absolute host path for the structured executor error report.
    pub error_report: PathBuf,
    /// Optional absolute host path for the structured executor result report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_report: Option<PathBuf>,
    /// Canonical materialization manifest bytes encoded as UTF-8 text.
    pub manifest: String,
    /// Optional hash mode requested by the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_report: Option<OwnershipHelperHashReport>,
    /// Logical-to-host id mapping configured by the parent.
    pub idmap: OwnershipHelperIdmap,
}

/// Parsed helper command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperCommand {
    /// Print protocol information as JSON.
    ProtocolInfo,
    /// Run ownership materialization from the given config path.
    Ownership {
        /// JSON config path.
        config: PathBuf,
        /// Optional inherited file descriptor to wait on before starting work.
        wait_fd: Option<RawFd>,
    },
    /// Wait for parent namespace setup, then exec the real ownership command.
    OwnershipTrampoline {
        /// JSON config path.
        config: PathBuf,
        /// Inherited file descriptor to wait on before exec.
        wait_fd: RawFd,
    },
}

/// Run the helper using process arguments.
pub fn main_from_env() -> ExitCode {
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
        HelperCommand::Ownership { config, wait_fd } => {
            if let Some(fd) = wait_fd {
                wait_for_parent(fd)?;
            }
            let config = read_config(&config)?;
            run_ownership_config(config)
        }
        HelperCommand::OwnershipTrampoline { config, wait_fd } => {
            wait_for_parent(wait_fd)?;
            exec_ownership_command(&config)
        }
    }
}

/// Parse helper command-line arguments.
pub fn parse_args(args: Vec<OsString>) -> Result<HelperCommand, String> {
    if args.len() == 1 && args[0] == "--protocol-info" {
        return Ok(HelperCommand::ProtocolInfo);
    }

    if args.first().is_some_and(|arg| arg == "ownership") {
        let mut config = None;
        let mut wait_fd = None;
        let mut index = 1;
        while index < args.len() {
            match args[index].to_str() {
                Some("--config") => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "--config requires a path".to_string())?;
                    config = Some(PathBuf::from(value));
                }
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
                Some(flag) => return Err(format!("unknown ownership argument '{flag}'")),
                None => return Err("ownership arguments must be UTF-8".to_string()),
            }
            index += 1;
        }
        let config = config.ok_or_else(|| "ownership requires --config".to_string())?;
        return Ok(HelperCommand::Ownership { config, wait_fd });
    }

    if args
        .first()
        .is_some_and(|arg| arg == "ownership-trampoline")
    {
        let mut config = None;
        let mut wait_fd = None;
        let mut index = 1;
        while index < args.len() {
            match args[index].to_str() {
                Some("--config") => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "--config requires a path".to_string())?;
                    config = Some(PathBuf::from(value));
                }
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
                Some(flag) => {
                    return Err(format!("unknown ownership-trampoline argument '{flag}'"));
                }
                None => return Err("ownership-trampoline arguments must be UTF-8".to_string()),
            }
            index += 1;
        }
        let config = config.ok_or_else(|| "ownership-trampoline requires --config".to_string())?;
        let wait_fd =
            wait_fd.ok_or_else(|| "ownership-trampoline requires --wait-fd".to_string())?;
        return Ok(HelperCommand::OwnershipTrampoline { config, wait_fd });
    }

    Err(format!(
        "usage: {HELPER_BINARY_NAME} --protocol-info | ownership --config PATH [--wait-fd FD]"
    ))
}

fn read_config(path: &Path) -> Result<OwnershipHelperConfig, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read helper config '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to parse helper config '{}': {error}",
            path.display()
        )
    })
}

fn run_ownership_config(config: OwnershipHelperConfig) -> Result<(), String> {
    let manifest = parse_manifest("manifest", &config.manifest, &config.error_report)?;
    let hash_report = match config.hash_report {
        Some(OwnershipHelperHashReport::TargetRoot) => Some(HashReport::TargetRoot),
        Some(OwnershipHelperHashReport::FsTreeObject {
            manifest,
            extra_files,
        }) => Some(HashReport::FsTreeObject {
            manifest: parse_manifest("hash manifest", &manifest, &config.error_report)?,
            extra_files,
        }),
        None => None,
    };

    let executor = OwnershipExecutor::with_paths_display_and_result(
        &manifest,
        config.target_root,
        PathBuf::from("/target"),
        config.error_report.clone(),
        config.result_report.clone(),
        hash_report,
    );

    match executor.apply() {
        Ok(result) => {
            if let (Some(path), Some(result)) = (&config.result_report, result) {
                write_executor_result_report_with_timings(
                    path,
                    result.object_hash,
                    Some(ExecutorResultTimings::from(result.timings)),
                )
                .map_err(|error| {
                    format!(
                        "failed to write executor result report '{}': {error}",
                        path.display()
                    )
                })?;
            }
            Ok(())
        }
        Err(report) => {
            write_executor_error_report(&config.error_report, &report).map_err(|error| {
                format!(
                    "failed to write executor error report '{}': {error}; original error: {report}",
                    config.error_report.display()
                )
            })?;
            Err(report.to_string())
        }
    }
}

fn parse_manifest(label: &str, text: &str, error_report: &Path) -> Result<FsTreeManifest, String> {
    FsTreeManifest::parse_canonical_bytes(text.as_bytes()).map_err(|error| {
        let report = ExecutorErrorReport {
            kind: "manifest".to_string(),
            path: error_report.display().to_string(),
            message: format!("failed to parse {label}: {error}"),
            errno: None,
        };
        let _ = write_executor_error_report(error_report, &report);
        report.to_string()
    })
}

fn wait_for_parent(fd: RawFd) -> Result<(), String> {
    let mut byte = [0_u8; 1];
    loop {
        let result = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if result == 1 {
            return Ok(());
        }
        if result == 0 {
            return Err(
                "parent closed namespace setup pipe before signalling readiness".to_string(),
            );
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("failed to read namespace setup pipe: {error}"));
        }
    }
}

fn exec_ownership_command(config: &Path) -> Result<(), String> {
    let current_exe = env::current_exe()
        .map_err(|error| format!("failed to resolve ownership helper executable: {error}"))?;
    let error = Command::new(current_exe)
        .arg("ownership")
        .arg("--config")
        .arg(config)
        .exec();
    Err(format!("failed to exec ownership helper: {error}"))
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
                OsString::from("--wait-fd"),
                OsString::from("7"),
            ])
            .unwrap(),
            HelperCommand::Ownership {
                config: PathBuf::from("/tmp/config.json"),
                wait_fd: Some(7),
            }
        );
    }

    #[test]
    fn parse_ownership_requires_config() {
        let error = parse_args(vec![OsString::from("ownership")]).unwrap_err();
        assert!(error.contains("--config"));
    }

    #[test]
    fn parse_trampoline_command() {
        assert_eq!(
            parse_args(vec![
                OsString::from("ownership-trampoline"),
                OsString::from("--config"),
                OsString::from("/tmp/config.json"),
                OsString::from("--wait-fd"),
                OsString::from("8"),
            ])
            .unwrap(),
            HelperCommand::OwnershipTrampoline {
                config: PathBuf::from("/tmp/config.json"),
                wait_fd: 8,
            }
        );
    }

    #[test]
    fn helper_config_serializes_idmap_and_hash_mode() {
        let config = OwnershipHelperConfig {
            target_root: PathBuf::from("/tmp/root"),
            error_report: PathBuf::from("/tmp/error.json"),
            result_report: Some(PathBuf::from("/tmp/result.json")),
            manifest: "{}\n".to_string(),
            hash_report: Some(OwnershipHelperHashReport::FsTreeObject {
                manifest: "{}\n".to_string(),
                extra_files: vec![(b"name".to_vec(), b"value".to_vec())],
            }),
            idmap: OwnershipHelperIdmap {
                current_uid: 1000,
                current_gid: 1000,
                subuid_base: 100000,
                subuid_count: 65536,
                subgid_base: 200000,
                subgid_count: 65536,
            },
        };

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value["target_root"], "/tmp/root");
        assert_eq!(value["idmap"]["subuid_base"], 100000);
        assert_eq!(value["hash_report"]["kind"], "fs_tree_object");

        let decoded: OwnershipHelperConfig = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, config);
    }
}
