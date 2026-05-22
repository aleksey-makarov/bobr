//! Ownership helper operation implementation and JSON protocol types.

use crate::executor::{
    ExecutorErrorReport, ExecutorResultTimings, write_executor_error_report,
    write_executor_result_report_with_timings,
};
use crate::idmap::MbuildIdmap;
use crate::ownership::{HashReport, OwnershipExecutor};
use mbuild_core::FsTreeManifest;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Serializable id mapping passed to the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OwnershipHelperIdmap {
    /// Host uid mapped to logical uid 0.
    pub(crate) current_uid: u32,
    /// Host gid mapped to logical gid 0.
    pub(crate) current_gid: u32,
    /// First host uid mapped to positive logical uids.
    pub(crate) subuid_base: u32,
    /// Number of positive logical uids available.
    pub(crate) subuid_count: u32,
    /// First host gid mapped to positive logical gids.
    pub(crate) subgid_base: u32,
    /// Number of positive logical gids available.
    pub(crate) subgid_count: u32,
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
pub(crate) enum OwnershipHelperHashReport {
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
pub(crate) struct OwnershipHelperConfig {
    /// Absolute host path to the target root.
    pub(crate) target_root: PathBuf,
    /// Absolute host path for the structured executor error report.
    pub(crate) error_report: PathBuf,
    /// Optional absolute host path for the structured executor result report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result_report: Option<PathBuf>,
    /// Canonical materialization manifest bytes encoded as UTF-8 text.
    pub(crate) manifest: String,
    /// Optional hash mode requested by the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hash_report: Option<OwnershipHelperHashReport>,
    /// Logical-to-host id mapping configured by the parent.
    pub(crate) idmap: OwnershipHelperIdmap,
}

pub(crate) fn run_config_path(path: &Path) -> Result<(), String> {
    let config = read_config(path)?;
    run_config(config)
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

fn run_config(config: OwnershipHelperConfig) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
