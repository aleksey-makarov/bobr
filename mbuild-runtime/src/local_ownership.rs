use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use crate::local_helper::{
    preflight_local_helper_runtime, run_local_helper_with_config, write_helper_manifest,
};
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
        |run_dir, error_report| {
            let manifest_path = run_dir.join("ownership-manifest.jsonl");
            write_helper_manifest(&manifest_path, manifest, "ownership manifest")?;
            let config = helper_config(&target_root, &manifest_path, idmap, error_report)?;
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
    manifest_path: &Path,
    idmap: &MbuildIdmap,
    error_report: &Path,
) -> Result<OwnershipHelperConfig, RuntimeError> {
    Ok(OwnershipHelperConfig {
        target_root: target_root.to_path_buf(),
        error_report: error_report.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
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
        let idmap = MbuildIdmap::for_tests(1000, 1001, 100000, 3, 200000, 4);
        let config = helper_config(
            Path::new("/tmp/root"),
            Path::new("/tmp/manifest.jsonl"),
            &idmap,
            Path::new("/tmp/error.json"),
        )
        .unwrap();

        assert_eq!(config.target_root, PathBuf::from("/tmp/root"));
        assert_eq!(config.manifest_path, PathBuf::from("/tmp/manifest.jsonl"));
        assert_eq!(config.idmap.current_gid, 1001);
    }
}
