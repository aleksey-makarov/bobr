use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use crate::local_helper::{preflight_local_helper_runtime, run_local_helper_with_config};
use mbuild_core::FsTreeManifest;
use mbuild_core::runtime_helper_protocol::{OwnershipHelperConfig, OwnershipHelperIdmap};
use std::fs;
use std::path::Path;

pub(crate) fn preflight_local_ownership_runtime(idmap: &MbuildIdmap) -> Result<(), RuntimeError> {
    preflight_local_helper_runtime(idmap)
}

pub(crate) fn run_local_ownership(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    workspace: &Path,
) -> Result<(), RuntimeError> {
    let target_root = fs::canonicalize(target_root)?;
    run_local_helper_with_config(
        idmap,
        workspace,
        "ownership",
        "ownership-helper.json",
        |error_report| {
            let config = helper_config(&target_root, manifest, idmap, error_report)?;
            serde_json::to_vec(&config).map_err(|error| {
                RuntimeError::Executor(format!(
                    "failed to serialize ownership helper config: {error}"
                ))
            })
        },
    )
}

fn helper_config(
    target_root: &Path,
    manifest: &FsTreeManifest,
    idmap: &MbuildIdmap,
    error_report: &Path,
) -> Result<OwnershipHelperConfig, RuntimeError> {
    let manifest = String::from_utf8(manifest.to_canonical_bytes().map_err(|error| {
        RuntimeError::InvalidInput(format!("failed to serialize ownership manifest: {error}"))
    })?)
    .expect("canonical fs-tree manifest is UTF-8");

    Ok(OwnershipHelperConfig {
        target_root: target_root.to_path_buf(),
        error_report: error_report.to_path_buf(),
        manifest,
        idmap: OwnershipHelperIdmap {
            current_uid: idmap.current_uid(),
            current_gid: idmap.current_gid(),
            subuid_base: idmap.subuid_base(),
            subuid_count: idmap.subuid_count(),
            subgid_base: idmap.subgid_base(),
            subgid_count: idmap.subgid_count(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn helper_config_serializes_manifest_and_idmap() {
        let manifest = FsTreeManifest::from_entries(vec![mbuild_core::FsTreeEntry::directory(
            "", 0, 0, 0o755,
        )])
        .unwrap();
        let idmap = MbuildIdmap::for_tests(1000, 1001, 100000, 3, 200000, 4);
        let config = helper_config(
            Path::new("/tmp/root"),
            &manifest,
            &idmap,
            Path::new("/tmp/error.json"),
        )
        .unwrap();

        assert_eq!(config.target_root, PathBuf::from("/tmp/root"));
        assert_eq!(config.idmap.current_gid, 1001);
        assert!(config.manifest.ends_with('\n'));
    }
}
