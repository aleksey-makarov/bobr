use crate::error::RuntimeError;
use crate::idmap::MbuildIdmap;
use crate::local_helper::{
    LocalHelperOperation, preflight_local_helper_runtime, run_local_helper_operation,
    write_helper_manifest,
};
use mbuild_core::FsTreeManifest;
use mbuild_core::runtime_helper_protocol::{OwnershipHelperConfig, OwnershipHelperIdmap};
use std::fs;
use std::path::{Path, PathBuf};

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
    run_local_helper_operation(
        idmap,
        workspace,
        OwnershipOperation {
            target_root,
            manifest,
            idmap,
        },
    )
}

struct OwnershipOperation<'a> {
    target_root: PathBuf,
    manifest: &'a FsTreeManifest,
    idmap: &'a MbuildIdmap,
}

impl LocalHelperOperation for OwnershipOperation<'_> {
    type Config = OwnershipHelperConfig;

    const COMMAND: &'static str = "ownership";
    const CONFIG_FILE: &'static str = "ownership-helper.json";
    const CONFIG_LABEL: &'static str = "ownership helper config";

    fn build_config(
        &self,
        run_dir: &Path,
        error_report: &Path,
    ) -> Result<Self::Config, RuntimeError> {
        let manifest_path = run_dir.join("ownership-manifest.jsonl");
        write_helper_manifest(&manifest_path, self.manifest, "ownership manifest")?;
        Ok(OwnershipHelperConfig {
            target_root: self.target_root.clone(),
            error_report: error_report.to_path_buf(),
            manifest_path,
            idmap: OwnershipHelperIdmap {
                current_uid: self.idmap.current_uid(),
                current_gid: self.idmap.current_gid(),
                subuid_base: self.idmap.subuid_base(),
                subuid_count: self.idmap.subuid_count(),
                subgid_base: self.idmap.subgid_base(),
                subgid_count: self.idmap.subgid_count(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::FsTreeEntry;
    use tempfile::tempdir;

    #[test]
    fn ownership_operation_builds_config_with_manifest_and_idmap() {
        let temp = tempdir().unwrap();
        let idmap = MbuildIdmap::for_tests(1000, 1001, 100000, 3, 200000, 4);
        let manifest =
            FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap();
        let operation = OwnershipOperation {
            target_root: PathBuf::from("/tmp/root"),
            manifest: &manifest,
            idmap: &idmap,
        };
        let config = operation
            .build_config(temp.path(), &temp.path().join("error.json"))
            .unwrap();

        assert_eq!(config.target_root, PathBuf::from("/tmp/root"));
        assert_eq!(
            config.manifest_path,
            temp.path().join("ownership-manifest.jsonl")
        );
        assert!(config.manifest_path.is_file());
        assert_eq!(config.idmap.current_gid, 1001);
    }
}
