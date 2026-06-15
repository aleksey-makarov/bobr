use super::archive::{
    write_composed_fs_tree_initramfs_host, write_composed_fs_tree_tar_host,
    write_composed_fs_tree_tar_stream,
};
use super::erofs::{
    ErofsRootfsBuilder, ErofsRootfsConfig, ErofsTarWriter, ProgramResolver, build_erofs_rootfs,
};
use super::initramfs::{InitramfsBuilder, InitramfsConfig, InitramfsWriter, build_initramfs};
use super::install::{InstallAttrs, InstallMeta, InstallRule, compile_install_rules};
use super::legacy_object::{
    FsTreeObjectMaterializer, OwnershipMaterializer, create_symlink, elapsed_ms,
    load_fs_tree_compose_input, map_fs_tree_error, set_mode, validate_tree_merge_file_attrs,
};
use super::merge::{TreeMergeBuilder, TreeMergeConfig, build_tree_merge};
use super::subset::{TreeSubsetBuilder, TreeSubsetConfig, build_tree_subset};
use super::tree::{
    TreeBuilder, TreeConfig, TreeEntry, TreePayload, apply_directory_modes_post_order, build_tree,
    fs_tree_manifest_for_entries, normalize_entries,
};
use crate::{BuildContext, Builder, BuilderInputObject, BuilderInputs, StagedBuildResult};
use fsobj_hash::{hash_file_bytes, hash_path, hash_symlink_node};
use mbuild_core::{
    BuildLogEvent, BuildLogger, BuilderError, ComposedFsTree, ComposedFsTreeEntry,
    FsTreeComposeInput, FsTreeEntry, FsTreeManifest, FsTreeObjectError, FsTreeOwnerMap,
    compose_fs_trees, create_fs_tree_staging_dir, validate_fs_tree_object,
};
use mbuild_runtime::FsTreeMaterializeReport;
use serde_json::Value;
use std::cell::RefCell;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tempfile::tempdir;

#[derive(Debug, Clone, Copy)]
struct CurrentOwnerMaterializer;

#[derive(Debug, Clone, Copy)]
struct FixedHashMaterializer;

#[derive(Debug)]
struct RecordingMaterializer {
    materialized_paths: RefCell<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
struct HostFsTreeObjectMaterializer {
    fail_hardlinks: bool,
}

#[derive(Debug, Default)]
struct RecordingBuildLogger {
    events: Mutex<Vec<BuildLogEvent>>,
}

impl RecordingBuildLogger {
    fn events(&self) -> Vec<BuildLogEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl BuildLogger for RecordingBuildLogger {
    fn log_event(&self, event: BuildLogEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn allocate_raw_log_path(&self, _label: &str) -> Result<PathBuf, String> {
        Err("recording logger does not allocate raw logs".to_string())
    }
}

impl OwnershipMaterializer for CurrentOwnerMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        object_dir: &Path,
        manifest: &FsTreeManifest,
        _temp_dir: &Path,
    ) -> Result<(), BuilderError> {
        let owner_map = current_owner_map(root_dir)?;
        for entry in manifest.entries() {
            let (uid, gid) = match entry {
                FsTreeEntry::File { uid, gid, .. }
                | FsTreeEntry::Directory { uid, gid, .. }
                | FsTreeEntry::Symlink { uid, gid, .. } => (*uid, *gid),
            };
            if uid != 0 || gid != 0 {
                return Err(BuilderError::ExecutionFailed(format!(
                    "test materializer supports only logical uid=0,gid=0, got uid={uid},gid={gid} for '{}'",
                    entry.path()
                )));
            }
        }
        validate_fs_tree_object(object_dir, &owner_map).map_err(map_fs_tree_error)?;
        Ok(())
    }
}

impl FsTreeObjectMaterializer for HostFsTreeObjectMaterializer {
    fn materialize_from_sources(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_object_dir: &Path,
        _workspace: &Path,
    ) -> Result<FsTreeMaterializeReport, BuilderError> {
        materialize_fs_tree_host_for_tests(
            inputs,
            composed,
            output_object_dir,
            self.fail_hardlinks,
            true,
        )
    }
}

impl FsTreeObjectMaterializer for FixedHashMaterializer {
    fn materialize_from_sources(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_object_dir: &Path,
        _workspace: &Path,
    ) -> Result<FsTreeMaterializeReport, BuilderError> {
        materialize_fs_tree_host_for_tests(inputs, composed, output_object_dir, false, false)
    }
}

impl FsTreeObjectMaterializer for RecordingMaterializer {
    fn materialize_from_sources(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_object_dir: &Path,
        _workspace: &Path,
    ) -> Result<FsTreeMaterializeReport, BuilderError> {
        self.materialized_paths.replace(
            composed
                .manifest()
                .entries()
                .iter()
                .map(|entry| entry.path().to_string())
                .collect(),
        );
        materialize_fs_tree_host_for_tests(inputs, composed, output_object_dir, false, true)
    }
}

#[derive(Debug, Clone, Copy)]
struct CurrentOwnerMap {
    uid: u32,
    gid: u32,
}

impl FsTreeOwnerMap for CurrentOwnerMap {
    fn physical_uid(&self, logical_uid: u32) -> Result<u32, FsTreeObjectError> {
        if logical_uid == 0 {
            Ok(self.uid)
        } else {
            Err(FsTreeObjectError::Invalid(format!(
                "test owner map supports only logical uid 0, got {logical_uid}"
            )))
        }
    }

    fn physical_gid(&self, logical_gid: u32) -> Result<u32, FsTreeObjectError> {
        if logical_gid == 0 {
            Ok(self.gid)
        } else {
            Err(FsTreeObjectError::Invalid(format!(
                "test owner map supports only logical gid 0, got {logical_gid}"
            )))
        }
    }
}

impl TreeBuilder {
    fn build_typed_for_tests(
        &self,
        config: TreeConfig,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree(config, inputs, cx, &CurrentOwnerMaterializer)
    }
}

impl TreeSubsetBuilder {
    fn build_typed_for_tests(
        &self,
        config: TreeSubsetConfig,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree_subset(
            config,
            inputs,
            cx,
            &HostFsTreeObjectMaterializer {
                fail_hardlinks: false,
            },
        )
    }
}

impl TreeMergeBuilder {
    fn build_typed_for_tests(
        &self,
        config: TreeMergeConfig,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_tree_merge(
            config,
            inputs,
            cx,
            &HostFsTreeObjectMaterializer {
                fail_hardlinks: false,
            },
        )
    }
}

impl ErofsRootfsBuilder {
    fn build_typed_for_tests(
        &self,
        config: ErofsRootfsConfig,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
        mkfs_erofs: PathBuf,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_erofs_rootfs(
            config,
            inputs,
            cx,
            &HostErofsTarWriter,
            &FixedProgramResolver { path: mkfs_erofs },
        )
    }
}

impl InitramfsBuilder {
    fn build_typed_for_tests(
        &self,
        config: InitramfsConfig,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        build_initramfs(config, inputs, cx, &HostInitramfsWriter)
    }
}

#[derive(Debug, Clone, Copy)]
struct HostErofsTarWriter;

impl ErofsTarWriter for HostErofsTarWriter {
    fn write_tar(
        &self,
        _inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_tar: &Path,
        _workspace: &Path,
    ) -> Result<(), BuilderError> {
        write_composed_fs_tree_tar_host(composed, output_tar)
    }
}

#[derive(Debug, Clone, Copy)]
struct HostInitramfsWriter;

impl InitramfsWriter for HostInitramfsWriter {
    fn write_initramfs(
        &self,
        _inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_initramfs: &Path,
        _workspace: &Path,
    ) -> Result<(), BuilderError> {
        write_composed_fs_tree_initramfs_host(composed, output_initramfs)
    }
}

#[derive(Debug, Clone)]
struct FixedProgramResolver {
    path: PathBuf,
}

impl ProgramResolver for FixedProgramResolver {
    fn resolve(&self, _program: &str) -> Result<PathBuf, BuilderError> {
        Ok(self.path.clone())
    }
}

#[derive(Debug, Clone, Copy)]
struct MissingProgramResolver;

impl ProgramResolver for MissingProgramResolver {
    fn resolve(&self, program: &str) -> Result<PathBuf, BuilderError> {
        Err(BuilderError::ExecutionFailed(format!(
            "required tool '{program}' was not found in PATH; install erofs-utils"
        )))
    }
}

fn build_context(root: &std::path::Path) -> BuildContext {
    let temp_dir = root.join("tree").join("tmp");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    BuildContext::with_noop_logger(temp_dir)
}

fn build_context_with_recording_logger(
    root: &std::path::Path,
) -> (BuildContext, Arc<RecordingBuildLogger>) {
    let logger = Arc::new(RecordingBuildLogger::default());
    let cx = build_context(root).with_logger(logger.clone());
    (cx, logger)
}

fn detail_u64(event: &BuildLogEvent, key: &str) -> u64 {
    event
        .details
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing numeric detail '{key}' in {event:?}"))
}

fn sample_install() -> InstallMeta {
    InstallMeta {
        rules: vec![InstallRule {
            path: "**".to_string(),
            attrs: InstallAttrs {
                uid: Some(0),
                gid: Some(0),
                directory_mode: Some(0o755),
                regular_file_mode: Some(0o644),
                executable_file_mode: Some(0o755),
                symlink_mode: Some(0o777),
            },
        }],
    }
}

fn install_with_attrs(
    uid: u32,
    gid: u32,
    directory_mode: u32,
    regular_file_mode: u32,
) -> InstallMeta {
    InstallMeta {
        rules: vec![InstallRule {
            path: "**".to_string(),
            attrs: InstallAttrs {
                uid: Some(uid),
                gid: Some(gid),
                directory_mode: Some(directory_mode),
                regular_file_mode: Some(regular_file_mode),
                executable_file_mode: Some(0o755),
                symlink_mode: None,
            },
        }],
    }
}

fn install_with_modes(directory_mode: u32, regular_file_mode: u32) -> InstallMeta {
    install_with_attrs(0, 0, directory_mode, regular_file_mode)
}

fn fs_tree_root(result: &StagedBuildResult) -> PathBuf {
    result.staged_path.join("root")
}

fn fs_tree_manifest(result: &StagedBuildResult) -> FsTreeManifest {
    FsTreeManifest::read_canonical(&result.staged_path.join("manifest.jsonl")).unwrap()
}

fn assert_valid_fs_tree(result: &StagedBuildResult) {
    let owner_map = current_owner_map(&result.staged_path.join("root")).unwrap();
    validate_fs_tree_object(&result.staged_path, &owner_map).unwrap();
    if let Some(object_hash) = result.object_hash {
        assert_eq!(object_hash, hash_path(&result.staged_path).unwrap());
    }
}

fn build_fs_tree_for_tests(
    root: &Path,
    name: &str,
    entries: Vec<TreeEntry>,
    install: InstallMeta,
) -> StagedBuildResult {
    let builder = TreeBuilder;
    let mut cx = build_context(&root.join(name));
    builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload { entries },
                install: Some(install),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap()
}

fn tree_merge_inputs(inputs: &[(&str, &StagedBuildResult)]) -> BuilderInputs {
    let mut builder_inputs = BuilderInputs::empty();
    for (name, result) in inputs {
        builder_inputs.insert(
            *name,
            BuilderInputObject {
                path: result.staged_path.clone(),
            },
        );
    }
    builder_inputs
}

fn install_fake_mkfs_erofs(dir: &Path, log_path: &Path, fail: bool) -> PathBuf {
    let script_path = dir.join("mkfs.erofs");
    let failure = if fail {
        "echo simulated mkfs failure >&2\nexit 17\n"
    } else {
        ""
    };
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > {}\n{failure}last=''\nprev=''\nfor arg in \"$@\"; do\n  prev=\"$last\"\n  last=\"$arg\"\ndone\nprintf 'fake erofs image\\n' > \"$prev\"\n",
            shell_quote(log_path)
        ),
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    script_path
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn read_fake_mkfs_args(log_path: &Path) -> Vec<String> {
    fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn tree_subset_config(patterns: &[&str]) -> TreeSubsetConfig {
    TreeSubsetConfig {
        include: patterns.iter().map(|pattern| pattern.to_string()).collect(),
    }
}

fn apply_test_modes(manifest: &FsTreeManifest, root_dir: &Path) -> Result<(), BuilderError> {
    for entry in manifest.entries() {
        if let FsTreeEntry::File { path, mode, .. } = entry {
            set_mode(&root_dir.join(path), *mode)?;
        }
    }
    apply_directory_modes_post_order(manifest, root_dir)
}

fn materialize_fs_tree_host_for_tests(
    inputs: &[FsTreeComposeInput],
    composed: &ComposedFsTree,
    output_object_dir: &Path,
    fail_hardlinks: bool,
    validate_after: bool,
) -> Result<FsTreeMaterializeReport, BuilderError> {
    let existed_before = output_object_dir.exists() || output_object_dir.is_symlink();
    let result = materialize_fs_tree_host_for_tests_inner(
        inputs,
        composed,
        output_object_dir,
        fail_hardlinks,
        validate_after,
    );
    if result.is_err()
        && !existed_before
        && (output_object_dir.exists() || output_object_dir.is_symlink())
    {
        let _ = fs::remove_dir_all(output_object_dir);
    }
    result
}

fn materialize_fs_tree_host_for_tests_inner(
    inputs: &[FsTreeComposeInput],
    composed: &ComposedFsTree,
    output_object_dir: &Path,
    fail_hardlinks: bool,
    validate_after: bool,
) -> Result<FsTreeMaterializeReport, BuilderError> {
    let paths = create_fs_tree_staging_dir(output_object_dir, composed.manifest())
        .map_err(map_fs_tree_error)?;
    let mut report = FsTreeMaterializeReport::default();

    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(composed.entries())
    {
        match (manifest_entry, composed_entry) {
            (FsTreeEntry::Directory { path, .. }, ComposedFsTreeEntry::Directory) => {
                let start = Instant::now();
                if !path.is_empty() {
                    fs::create_dir(paths.root_dir.join(path)).map_err(|error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to create test fs-tree directory '{}': {error}",
                            paths.root_dir.join(path).display()
                        ))
                    })?;
                }
                report.directory_ms += elapsed_ms(start);
                report.directory_count += 1;
            }
            (FsTreeEntry::File { path, .. }, ComposedFsTreeEntry::File { source_path }) => {
                let start = Instant::now();
                if fail_hardlinks {
                    return Err(BuilderError::ExecutionFailed(format!(
                        "failed to hardlink fs-tree file '{}' to '{}': {}",
                        source_path.display(),
                        paths.root_dir.join(path).display(),
                        io::Error::from(io::ErrorKind::PermissionDenied)
                    )));
                }
                fs::hard_link(source_path, paths.root_dir.join(path)).map_err(|error| {
                    BuilderError::ExecutionFailed(format!(
                        "failed to hardlink fs-tree file '{}' to '{}': {error}",
                        source_path.display(),
                        paths.root_dir.join(path).display()
                    ))
                })?;
                let owner_map = current_owner_map(&paths.root_dir.join(path))?;
                validate_tree_merge_file_attrs(
                    &paths.root_dir.join(path),
                    manifest_entry,
                    &owner_map,
                )?;
                report.hardlink_ms += elapsed_ms(start);
                report.file_count += 1;
                report.hardlinked_file_count += 1;
            }
            (FsTreeEntry::Symlink { path, target, .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                let start = Instant::now();
                create_symlink(target, &paths.root_dir.join(path))?;
                report.symlink_ms += elapsed_ms(start);
                report.symlink_count += 1;
            }
            _ => {
                return Err(BuilderError::ExecutionFailed(format!(
                    "test fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                )));
            }
        }
    }

    let start = Instant::now();
    apply_test_modes(composed.manifest(), &paths.root_dir)?;
    report.ownership_ms = elapsed_ms(start);
    if validate_after {
        let owner_map = current_owner_map(&paths.root_dir)?;
        validate_fs_tree_object(output_object_dir, &owner_map).map_err(map_fs_tree_error)?;
    }
    let _ = inputs;
    Ok(report)
}

#[cfg(unix)]
fn current_owner_map(path: &Path) -> Result<CurrentOwnerMap, BuilderError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to inspect fs-tree root '{}': {error}",
            path.display()
        ))
    })?;
    Ok(CurrentOwnerMap {
        uid: metadata.uid(),
        gid: metadata.gid(),
    })
}

#[cfg(not(unix))]
fn current_owner_map(_path: &Path) -> Result<CurrentOwnerMap, BuilderError> {
    Ok(CurrentOwnerMap { uid: 0, gid: 0 })
}

#[test]
fn tree_subset_selects_manifest_paths_and_recreates_symlinks() {
    let builder = TreeSubsetBuilder;
    let temp = tempdir().unwrap();
    let input = build_fs_tree_for_tests(
        temp.path(),
        "input",
        vec![
            TreeEntry::File {
                path: "usr/lib64/libfoo.so.1".to_string(),
                text: "runtime\n".to_string(),
                executable: true,
            },
            TreeEntry::Symlink {
                path: "usr/lib64/libfoo.so".to_string(),
                target: "libfoo.so.1".to_string(),
            },
            TreeEntry::File {
                path: "usr/bin/tool".to_string(),
                text: "tool\n".to_string(),
                executable: true,
            },
        ],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("subset"));

    let result = builder
        .build_typed_for_tests(
            tree_subset_config(&["usr/lib64/libfoo.so*", "not-present/**"]),
            tree_merge_inputs(&[("tree", &input)]),
            &mut cx,
        )
        .unwrap();

    assert_eq!(
        fs::read_to_string(fs_tree_root(&result).join("usr/lib64/libfoo.so.1")).unwrap(),
        "runtime\n"
    );
    assert_eq!(
        fs::read_link(fs_tree_root(&result).join("usr/lib64/libfoo.so")).unwrap(),
        PathBuf::from("libfoo.so.1")
    );
    assert!(!fs_tree_root(&result).join("usr/bin/tool").exists());
    assert!(fs_tree_root(&result).join("usr").is_dir());
    assert!(fs_tree_root(&result).join("usr/lib64").is_dir());
    assert_valid_fs_tree(&result);

    let manifest_paths = fs_tree_manifest(&result)
        .entries()
        .iter()
        .map(|entry| entry.path().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        manifest_paths,
        vec![
            "",
            "usr",
            "usr/lib64",
            "usr/lib64/libfoo.so",
            "usr/lib64/libfoo.so.1"
        ]
    );
    for path in ["usr/lib64/libfoo.so", "usr/lib64/libfoo.so.1"] {
        assert!(
            fs_tree_manifest(&result)
                .entries()
                .iter()
                .find(|entry| entry.path() == path)
                .and_then(FsTreeEntry::leaf_hash)
                .is_some()
        );
    }
}

#[cfg(unix)]
#[test]
fn tree_subset_hardlinks_selected_files() {
    let builder = TreeSubsetBuilder;
    let temp = tempdir().unwrap();
    let input = build_fs_tree_for_tests(
        temp.path(),
        "input",
        vec![TreeEntry::File {
            path: "lib/libfoo.so.1".to_string(),
            text: "runtime\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("subset"));

    let result = builder
        .build_typed_for_tests(
            tree_subset_config(&["lib/libfoo.so*"]),
            tree_merge_inputs(&[("tree", &input)]),
            &mut cx,
        )
        .unwrap();

    let src = fs::metadata(fs_tree_root(&input).join("lib/libfoo.so.1")).unwrap();
    let dst = fs::metadata(fs_tree_root(&result).join("lib/libfoo.so.1")).unwrap();
    assert_eq!((src.dev(), src.ino()), (dst.dev(), dst.ino()));
}

#[test]
fn tree_subset_rejects_empty_result_but_allows_unmatched_patterns() {
    let builder = TreeSubsetBuilder;
    let temp = tempdir().unwrap();
    let input = build_fs_tree_for_tests(
        temp.path(),
        "input",
        vec![TreeEntry::File {
            path: "lib/libfoo.so.1".to_string(),
            text: "runtime\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("empty"));

    let error = builder
        .build_typed_for_tests(
            tree_subset_config(&["not-present/**"]),
            tree_merge_inputs(&[("tree", &input)]),
            &mut cx,
        )
        .unwrap_err();
    assert!(error.to_string().contains("selected no paths"));
}

#[test]
fn tree_subset_rejects_invalid_config_and_input() {
    let builder = TreeSubsetBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(&temp.path().join("missing"));
    let error = builder
        .build_typed_for_tests(
            tree_subset_config(&["lib/*"]),
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("required input slot 'tree' is missing")
    );

    let input = build_fs_tree_for_tests(
        temp.path(),
        "input",
        vec![TreeEntry::Dir {
            path: "lib".to_string(),
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("bad-pattern"));
    let error = builder
        .build_typed_for_tests(
            tree_subset_config(&["../lib/*"]),
            tree_merge_inputs(&[("tree", &input)]),
            &mut cx,
        )
        .unwrap_err();
    assert!(error.to_string().contains("must not contain '..'"));

    let not_tree = temp.path().join("not-tree");
    fs::write(&not_tree, b"not a tree").unwrap();
    let mut inputs = BuilderInputs::empty();
    inputs.insert("tree", BuilderInputObject { path: not_tree });
    let mut cx = build_context(&temp.path().join("not-tree-cx"));
    let error = builder
        .build_typed_for_tests(tree_subset_config(&["lib/*"]), inputs, &mut cx)
        .unwrap_err();
    assert!(error.to_string().contains("is not a valid fs-tree object"));
}

#[test]
fn tree_subset_fails_when_hardlink_fails() {
    let temp = tempdir().unwrap();
    let input = build_fs_tree_for_tests(
        temp.path(),
        "input",
        vec![TreeEntry::File {
            path: "lib/libfoo.so.1".to_string(),
            text: "runtime\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("subset"));

    let error = build_tree_subset(
        tree_subset_config(&["lib/libfoo.so*"]),
        tree_merge_inputs(&[("tree", &input)]),
        &mut cx,
        &HostFsTreeObjectMaterializer {
            fail_hardlinks: true,
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to hardlink fs-tree file")
    );
}

#[test]
fn tree_subset_uses_manifest_without_discovering_input_tree() {
    let builder = TreeSubsetBuilder;
    let temp = tempdir().unwrap();
    let object_dir = temp.path().join("input-object");
    let manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("bin", 0, 0, 0o755),
        FsTreeEntry::file_with_hash("bin/tool", 0, 0, 0o755, hash_file_bytes(true, b"tool\n")),
        FsTreeEntry::directory("locked", 0, 0, 0o000),
    ])
    .unwrap();
    let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
    fs::create_dir(paths.root_dir.join("bin")).unwrap();
    fs::write(paths.root_dir.join("bin/tool"), b"tool\n").unwrap();
    fs::set_permissions(
        paths.root_dir.join("bin/tool"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let mut inputs = BuilderInputs::empty();
    inputs.insert("tree", BuilderInputObject { path: object_dir });
    let mut cx = build_context(&temp.path().join("subset"));

    let result = builder
        .build_typed_for_tests(tree_subset_config(&["bin/tool"]), inputs, &mut cx)
        .unwrap();

    assert_eq!(
        fs::read_to_string(fs_tree_root(&result).join("bin/tool")).unwrap(),
        "tool\n"
    );
    assert!(!fs_tree_root(&result).join("locked").exists());
    assert_valid_fs_tree(&result);
}

#[test]
fn erofs_rootfs_invokes_mkfs_without_optional_flags_and_publishes_file() {
    let builder = ErofsRootfsBuilder;
    let temp = tempdir().unwrap();
    let tree = build_fs_tree_for_tests(
        temp.path(),
        "tree",
        vec![TreeEntry::File {
            path: "etc/hostname".to_string(),
            text: "mbuild\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let log_path = temp.path().join("mkfs-args.txt");
    let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, false);
    let mut cx = build_context(&temp.path().join("erofs"));

    let result = builder
        .build_typed_for_tests(
            ErofsRootfsConfig {
                compression: None,
                label: None,
            },
            tree_merge_inputs(&[("tree", &tree)]),
            &mut cx,
            mkfs,
        )
        .unwrap();

    assert!(result.staged_path.is_file());
    assert_eq!(
        fs::read_to_string(&result.staged_path).unwrap(),
        "fake erofs image\n"
    );
    assert_eq!(result.object_hash, None);
    let args = read_fake_mkfs_args(&log_path);
    assert_eq!(
        &args[..6],
        ["--tar=f", "--sort=path", "-T", "0", "-U", "clear"]
    );
    assert!(!args.iter().any(|arg| arg == "-z"));
    assert!(!args.iter().any(|arg| arg == "-L"));
    assert!(args[args.len() - 2].ends_with(".erofs"));
    assert!(args[args.len() - 1].ends_with(".tar"));
}

#[test]
fn initramfs_writes_newc_file() {
    let builder = InitramfsBuilder;
    let temp = tempdir().unwrap();
    let tree = build_fs_tree_for_tests(
        temp.path(),
        "tree",
        vec![
            TreeEntry::File {
                path: "init".to_string(),
                text: "#!/bin/sh\n".to_string(),
                executable: true,
            },
            TreeEntry::Symlink {
                path: "bin/sh".to_string(),
                target: "../init".to_string(),
            },
        ],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("initramfs"));

    let result = builder
        .build_typed_for_tests(
            InitramfsConfig {},
            tree_merge_inputs(&[("tree", &tree)]),
            &mut cx,
        )
        .unwrap();

    assert!(result.staged_path.is_file());
    assert_eq!(result.object_hash, None);
    let bytes = fs::read(&result.staged_path).unwrap();
    assert!(bytes.starts_with(b"070701"));
    assert!(
        bytes
            .windows("#!/bin/sh\n".len())
            .any(|window| window == b"#!/bin/sh\n")
    );
    assert!(
        bytes
            .windows("TRAILER!!!".len())
            .any(|window| window == b"TRAILER!!!")
    );
}

#[test]
fn initramfs_allows_identical_duplicate_file_overlap() {
    let builder = InitramfsBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "etc/same".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "etc/same".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("initramfs"));

    let result = builder
        .build_typed_for_tests(
            InitramfsConfig {},
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
        )
        .unwrap();

    assert!(result.staged_path.is_file());
}

#[test]
fn initramfs_rejects_zero_inputs() {
    let temp = tempdir().unwrap();
    let mut cx = build_context(&temp.path().join("initramfs"));

    let error = InitramfsBuilder
        .build_typed_for_tests(InitramfsConfig {}, BuilderInputs::empty(), &mut cx)
        .unwrap_err();

    assert!(error.to_string().contains("requires at least one"));
}

#[test]
fn erofs_rootfs_allows_identical_duplicate_file_overlap() {
    let builder = ErofsRootfsBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "usr/lib64/libsame.so.1".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "usr/lib64/libsame.so.1".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let log_path = temp.path().join("mkfs-args.txt");
    let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, false);
    let mut cx = build_context(&temp.path().join("erofs"));

    let result = builder
        .build_typed_for_tests(
            ErofsRootfsConfig {
                compression: None,
                label: None,
            },
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
            mkfs,
        )
        .unwrap();

    assert!(result.staged_path.is_file());
    assert_eq!(
        fs::read_to_string(&result.staged_path).unwrap(),
        "fake erofs image\n"
    );
}

#[test]
fn erofs_rootfs_passes_compression_and_label_to_mkfs() {
    let builder = ErofsRootfsBuilder;
    let temp = tempdir().unwrap();
    let tree = build_fs_tree_for_tests(
        temp.path(),
        "tree",
        vec![TreeEntry::Dir {
            path: "var".to_string(),
        }],
        sample_install(),
    );
    let log_path = temp.path().join("mkfs-args.txt");
    let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, false);
    let mut cx = build_context(&temp.path().join("erofs"));

    builder
        .build_typed_for_tests(
            ErofsRootfsConfig {
                compression: Some("lz4hc,12".to_string()),
                label: Some("rootfs".to_string()),
            },
            tree_merge_inputs(&[("tree", &tree)]),
            &mut cx,
            mkfs,
        )
        .unwrap();

    let args = read_fake_mkfs_args(&log_path);
    assert!(args.windows(2).any(|window| window == ["-L", "rootfs"]));
    assert!(args.windows(2).any(|window| window == ["-z", "lz4hc,12"]));
}

#[test]
fn erofs_rootfs_rejects_zero_inputs() {
    let temp = tempdir().unwrap();
    let mkfs = install_fake_mkfs_erofs(temp.path(), &temp.path().join("mkfs-args.txt"), false);
    let mut cx = build_context(&temp.path().join("erofs"));

    let error = ErofsRootfsBuilder
        .build_typed_for_tests(
            ErofsRootfsConfig {
                compression: None,
                label: None,
            },
            BuilderInputs::empty(),
            &mut cx,
            mkfs,
        )
        .unwrap_err();

    assert!(error.to_string().contains("requires at least one"));
}

#[test]
fn erofs_rootfs_rejects_empty_compression_and_label() {
    let temp = tempdir().unwrap();
    let mkfs = install_fake_mkfs_erofs(temp.path(), &temp.path().join("mkfs-args.txt"), false);
    let mut cx = build_context(&temp.path().join("compression"));

    let error = ErofsRootfsBuilder
        .build_typed_for_tests(
            ErofsRootfsConfig {
                compression: Some(String::new()),
                label: None,
            },
            BuilderInputs::empty(),
            &mut cx,
            mkfs.clone(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("compression"));

    let mut cx = build_context(&temp.path().join("label"));
    let error = ErofsRootfsBuilder
        .build_typed_for_tests(
            ErofsRootfsConfig {
                compression: None,
                label: Some(String::new()),
            },
            BuilderInputs::empty(),
            &mut cx,
            mkfs,
        )
        .unwrap_err();
    assert!(error.to_string().contains("label"));
}

#[test]
fn erofs_rootfs_reports_missing_mkfs_erofs() {
    let temp = tempdir().unwrap();
    let tree = build_fs_tree_for_tests(
        temp.path(),
        "tree",
        vec![TreeEntry::Dir {
            path: "var".to_string(),
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("erofs"));

    let error = build_erofs_rootfs(
        ErofsRootfsConfig {
            compression: None,
            label: None,
        },
        tree_merge_inputs(&[("tree", &tree)]),
        &mut cx,
        &HostErofsTarWriter,
        &MissingProgramResolver,
    )
    .unwrap_err();

    assert!(error.to_string().contains("mkfs.erofs"));
    assert!(error.to_string().contains("erofs-utils"));
}

#[test]
fn erofs_rootfs_reports_mkfs_stderr_on_failure() {
    let temp = tempdir().unwrap();
    let tree = build_fs_tree_for_tests(
        temp.path(),
        "tree",
        vec![TreeEntry::Dir {
            path: "var".to_string(),
        }],
        sample_install(),
    );
    let log_path = temp.path().join("mkfs-args.txt");
    let mkfs = install_fake_mkfs_erofs(temp.path(), &log_path, true);
    let mut cx = build_context(&temp.path().join("erofs"));

    let error = ErofsRootfsBuilder
        .build_typed_for_tests(
            ErofsRootfsConfig {
                compression: None,
                label: None,
            },
            tree_merge_inputs(&[("tree", &tree)]),
            &mut cx,
            mkfs,
        )
        .unwrap_err();

    assert!(error.to_string().contains("mkfs.erofs failed"));
    assert!(error.to_string().contains("simulated mkfs failure"));
}

#[test]
fn erofs_tar_generation_uses_manifest_metadata_order_and_sources() {
    let temp = tempdir().unwrap();
    let left_manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("usr", 11, 12, 0o755),
        FsTreeEntry::directory("usr/bin", 11, 12, 0o755),
        FsTreeEntry::file("usr/bin/tool", 11, 12, 0o755),
    ])
    .unwrap();
    let left_paths = create_fs_tree_staging_dir(&temp.path().join("left"), &left_manifest).unwrap();
    fs::create_dir(left_paths.root_dir.join("usr")).unwrap();
    fs::create_dir(left_paths.root_dir.join("usr/bin")).unwrap();
    fs::write(left_paths.root_dir.join("usr/bin/tool"), b"tool\n").unwrap();

    let right_manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("etc", 21, 22, 0o750),
        FsTreeEntry::file("etc/config", 21, 22, 0o640),
        FsTreeEntry::symlink("etc/tool-link", 21, 22, "../usr/bin/tool"),
    ])
    .unwrap();
    let right_paths =
        create_fs_tree_staging_dir(&temp.path().join("right"), &right_manifest).unwrap();
    fs::create_dir(right_paths.root_dir.join("etc")).unwrap();
    fs::write(right_paths.root_dir.join("etc/config"), b"config\n").unwrap();
    create_symlink(
        "../usr/bin/tool",
        &right_paths.root_dir.join("etc/tool-link"),
    )
    .unwrap();
    let compose_inputs = vec![
        load_fs_tree_compose_input(&right_paths.object_dir).unwrap(),
        load_fs_tree_compose_input(&left_paths.object_dir).unwrap(),
    ];
    let composed = compose_fs_trees(&compose_inputs).unwrap();
    let mut bytes = Vec::new();

    write_composed_fs_tree_tar_stream(&mut bytes, &composed).unwrap();

    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut seen = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let header = entry.header().clone();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let mut contents = Vec::new();
        io::copy(&mut entry, &mut contents).unwrap();
        seen.push((
            path,
            header.entry_type(),
            header.uid().unwrap(),
            header.gid().unwrap(),
            header.mode().unwrap(),
            header.mtime().unwrap(),
            contents,
            header.link_name().unwrap().map(|path| path.into_owned()),
        ));
    }

    assert_eq!(
        seen.iter()
            .map(|entry| entry.0.as_str())
            .collect::<Vec<_>>(),
        vec![
            "etc/",
            "etc/config",
            "etc/tool-link",
            "usr/",
            "usr/bin/",
            "usr/bin/tool"
        ]
    );
    assert_eq!(
        (seen[0].2, seen[0].3, seen[0].4, seen[0].5),
        (21, 22, 0o750, 0)
    );
    assert_eq!(seen[1].1, tar::EntryType::Regular);
    assert_eq!(
        (seen[1].2, seen[1].3, seen[1].4, seen[1].5),
        (21, 22, 0o640, 0)
    );
    assert_eq!(seen[1].6, b"config\n");
    assert_eq!(seen[2].1, tar::EntryType::Symlink);
    assert_eq!(
        (seen[2].2, seen[2].3, seen[2].4, seen[2].5),
        (21, 22, 0o777, 0)
    );
    assert_eq!(seen[2].7.as_deref(), Some(Path::new("../usr/bin/tool")));
    assert_eq!(
        (seen[5].2, seen[5].3, seen[5].4, seen[5].5),
        (11, 12, 0o755, 0)
    );
    assert_eq!(seen[5].6, b"tool\n");
}

#[test]
fn tree_merge_requires_at_least_two_inputs() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_typed_for_tests(TreeMergeConfig {}, BuilderInputs::empty(), &mut cx)
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("requires at least two fs-tree inputs")
    );
}

#[test]
fn tree_merge_rejects_non_fs_tree_input() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::Dir {
            path: "left".to_string(),
        }],
        sample_install(),
    );
    let not_tree = temp.path().join("not-tree");
    fs::write(&not_tree, b"not a tree").unwrap();
    let mut inputs = tree_merge_inputs(&[("left", &left)]);
    inputs.insert("bad", BuilderInputObject { path: not_tree });
    let mut cx = build_context(&temp.path().join("merge"));

    let error = builder
        .build_typed_for_tests(TreeMergeConfig {}, inputs, &mut cx)
        .unwrap_err();

    assert!(error.to_string().contains("is not a valid fs-tree object"));
}

#[test]
fn tree_merge_combines_disjoint_fs_trees() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "bin/left".to_string(),
            text: "left\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "etc/right.conf".to_string(),
            text: "right\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let result = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("right", &right), ("left", &left)]),
            &mut cx,
        )
        .unwrap();

    assert_eq!(
        fs::read_to_string(fs_tree_root(&result).join("bin/left")).unwrap(),
        "left\n"
    );
    assert_eq!(
        fs::read_to_string(fs_tree_root(&result).join("etc/right.conf")).unwrap(),
        "right\n"
    );
    assert!(
        fs_tree_manifest(&result)
            .entries()
            .iter()
            .any(|entry| entry.path() == "bin")
    );
    assert_valid_fs_tree(&result);
}

#[test]
fn tree_merge_logs_stage_counts_and_hash_event() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "bin/left".to_string(),
            text: "left\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "etc/right.conf".to_string(),
            text: "right\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let (mut cx, logger) = build_context_with_recording_logger(&temp.path().join("merge"));

    let result = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("right", &right), ("left", &left)]),
            &mut cx,
        )
        .unwrap();
    assert_valid_fs_tree(&result);
    for path in ["bin/left", "etc/right.conf"] {
        assert!(
            fs_tree_manifest(&result)
                .entries()
                .iter()
                .find(|entry| entry.path() == path)
                .and_then(FsTreeEntry::leaf_hash)
                .is_some()
        );
    }

    let events = logger.events();
    let phases = events
        .iter()
        .map(|event| event.phase.as_str())
        .collect::<Vec<_>>();
    assert!(phases.contains(&"compose-done"));
    assert!(phases.contains(&"stage-done"));
    assert!(phases.contains(&"ownership-done"));
    assert!(phases.contains(&"hash-done"));

    let stage = events
        .iter()
        .find(|event| event.phase == "stage-done")
        .unwrap();
    assert_eq!(detail_u64(stage, "file_count"), 2);
    assert_eq!(detail_u64(stage, "hardlinked_file_count"), 2);
    assert_eq!(detail_u64(stage, "copied_file_count"), 0);
    assert_eq!(detail_u64(stage, "symlink_count"), 0);
    assert!(detail_u64(stage, "directory_count") >= 3);

    let hash = events
        .iter()
        .find(|event| event.phase == "hash-done")
        .unwrap();
    assert!(hash.object_hash.is_some());
    assert!(hash.details.contains_key("duration_ms"));
}

#[cfg(unix)]
#[test]
fn tree_merge_does_not_scan_input_directories_during_manifest_compose() {
    let temp = tempdir().unwrap();
    let base_object = temp.path().join("base");
    let base_manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("locked", 0, 0, 0o000),
    ])
    .unwrap();
    let base_paths = create_fs_tree_staging_dir(&base_object, &base_manifest).unwrap();
    fs::create_dir(base_paths.root_dir.join("locked")).unwrap();
    fs::set_permissions(
        base_paths.root_dir.join("locked"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();

    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "bin/right".to_string(),
            text: "right\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let mut inputs = tree_merge_inputs(&[("right", &right)]);
    inputs.insert("base", BuilderInputObject { path: base_object });
    let mut cx = build_context(&temp.path().join("merge"));

    let result =
        build_tree_merge(TreeMergeConfig {}, inputs, &mut cx, &FixedHashMaterializer).unwrap();

    assert!(result.object_hash.is_some());
    assert!(fs_tree_root(&result).join("locked").is_dir());
    assert_eq!(
        fs::metadata(fs_tree_root(&result).join("locked"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o000
    );

    fs::set_permissions(
        base_paths.root_dir.join("locked"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::set_permissions(
        fs_tree_root(&result).join("locked"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn tree_merge_rejects_hardlink_source_attr_mismatch_without_mutating_source() {
    let temp = tempdir().unwrap();
    let base_object = temp.path().join("base");
    let base_manifest = FsTreeManifest::from_entries(vec![
        FsTreeEntry::directory("", 0, 0, 0o755),
        FsTreeEntry::directory("bin", 0, 0, 0o755),
        FsTreeEntry::file("bin/tool", 0, 0, 0o755),
    ])
    .unwrap();
    let base_paths = create_fs_tree_staging_dir(&base_object, &base_manifest).unwrap();
    fs::create_dir(base_paths.root_dir.join("bin")).unwrap();
    fs::write(base_paths.root_dir.join("bin/tool"), b"tool").unwrap();
    fs::set_permissions(
        base_paths.root_dir.join("bin/tool"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "etc/right.conf".to_string(),
            text: "right\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let mut inputs = tree_merge_inputs(&[("right", &right)]);
    inputs.insert("base", BuilderInputObject { path: base_object });
    let mut cx = build_context(&temp.path().join("merge"));

    let error = build_tree_merge(
        TreeMergeConfig {},
        inputs,
        &mut cx,
        &HostFsTreeObjectMaterializer {
            fail_hardlinks: false,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("has mode 644, expected 755"));
    assert_eq!(
        fs::metadata(base_paths.root_dir.join("bin/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
}

#[cfg(unix)]
#[test]
fn tree_merge_materializes_complete_manifest() {
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "bin/left".to_string(),
            text: "left\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "etc/right.conf".to_string(),
            text: "right\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge"));
    let materializer = RecordingMaterializer {
        materialized_paths: RefCell::new(Vec::new()),
    };

    let result = build_tree_merge(
        TreeMergeConfig {},
        tree_merge_inputs(&[("right", &right), ("left", &left)]),
        &mut cx,
        &materializer,
    )
    .unwrap();

    assert_eq!(
        result.object_hash,
        Some(hash_path(&result.staged_path).unwrap())
    );
    let materialized_paths = materializer.materialized_paths.borrow();
    assert!(materialized_paths.iter().any(|path| path.is_empty()));
    assert!(materialized_paths.iter().any(|path| path == "bin"));
    assert!(materialized_paths.iter().any(|path| path == "etc"));
    assert!(materialized_paths.iter().any(|path| path == "bin/left"));
    assert!(
        materialized_paths
            .iter()
            .any(|path| path == "etc/right.conf")
    );
}

#[test]
fn tree_merge_allows_matching_directory_overlap() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "usr/bin/left".to_string(),
            text: "left\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "usr/lib/right".to_string(),
            text: "right\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let result = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
        )
        .unwrap();

    assert!(fs_tree_root(&result).join("usr/bin/left").is_file());
    assert!(fs_tree_root(&result).join("usr/lib/right").is_file());
    assert_valid_fs_tree(&result);
}

#[test]
fn tree_merge_allows_identical_duplicate_file_overlap() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "usr/lib64/libsame.so.1".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "usr/lib64/libsame.so.1".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let result = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
        )
        .unwrap();

    assert_eq!(
        fs::read_to_string(fs_tree_root(&result).join("usr/lib64/libsame.so.1")).unwrap(),
        "same\n"
    );
    assert_eq!(
        fs_tree_manifest(&result)
            .entries()
            .iter()
            .filter(|entry| entry.path() == "usr/lib64/libsame.so.1")
            .count(),
        1
    );
    assert_valid_fs_tree(&result);
}

#[test]
fn tree_merge_allows_identical_duplicate_symlink_overlap() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::Symlink {
            path: "usr/lib64/libsame.so".to_string(),
            target: "libsame.so.1".to_string(),
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::Symlink {
            path: "usr/lib64/libsame.so".to_string(),
            target: "libsame.so.1".to_string(),
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let result = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
        )
        .unwrap();

    assert_eq!(
        fs::read_link(fs_tree_root(&result).join("usr/lib64/libsame.so")).unwrap(),
        Path::new("libsame.so.1")
    );
    assert_valid_fs_tree(&result);
}

#[test]
fn tree_merge_rejects_directory_attr_mismatch() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::Dir {
            path: "var".to_string(),
        }],
        install_with_modes(0o755, 0o644),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::Dir {
            path: "var".to_string(),
        }],
        install_with_modes(0o700, 0o644),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let error = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
        )
        .unwrap_err();

    assert!(error.to_string().contains("conflicting fs-tree entries"));
}

#[test]
fn tree_merge_rejects_non_identical_duplicate_files_and_symlinks() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left_file = build_fs_tree_for_tests(
        temp.path(),
        "left-file",
        vec![TreeEntry::File {
            path: "bin/tool".to_string(),
            text: "left\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let right_file = build_fs_tree_for_tests(
        temp.path(),
        "right-file",
        vec![TreeEntry::File {
            path: "bin/tool".to_string(),
            text: "right\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge-files"));
    let error = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left_file), ("right", &right_file)]),
            &mut cx,
        )
        .unwrap_err();
    assert!(error.to_string().contains("conflicting fs-tree entries"));

    let left_link = build_fs_tree_for_tests(
        temp.path(),
        "left-link",
        vec![TreeEntry::Symlink {
            path: "bin/tool".to_string(),
            target: "left".to_string(),
        }],
        sample_install(),
    );
    let right_link = build_fs_tree_for_tests(
        temp.path(),
        "right-link",
        vec![TreeEntry::Symlink {
            path: "bin/tool".to_string(),
            target: "right".to_string(),
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge-links"));
    let error = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left_link), ("right", &right_link)]),
            &mut cx,
        )
        .unwrap_err();
    assert!(error.to_string().contains("conflicting fs-tree entries"));
}

#[test]
fn tree_merge_rejects_duplicate_file_with_matching_hash_but_different_metadata() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "bin/tool".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        install_with_modes(0o755, 0o644),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::File {
            path: "bin/tool".to_string(),
            text: "same\n".to_string(),
            executable: false,
        }],
        install_with_modes(0o755, 0o600),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let error = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
        )
        .unwrap_err();

    assert!(error.to_string().contains("metadata differs"));
}

#[test]
fn tree_merge_rejects_leaf_parent_conflicts() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let leaf = build_fs_tree_for_tests(
        temp.path(),
        "leaf",
        vec![
            TreeEntry::File {
                path: "opt".to_string(),
                text: "leaf\n".to_string(),
                executable: false,
            },
            TreeEntry::Dir {
                path: "other".to_string(),
            },
        ],
        sample_install(),
    );
    let child = build_fs_tree_for_tests(
        temp.path(),
        "child",
        vec![TreeEntry::File {
            path: "opt/tool".to_string(),
            text: "child\n".to_string(),
            executable: false,
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let error = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("leaf", &leaf), ("child", &child)]),
            &mut cx,
        )
        .unwrap_err();

    assert!(error.to_string().contains("conflict"));
}

#[cfg(unix)]
#[test]
fn tree_merge_hardlinks_files_when_possible() {
    let builder = TreeMergeBuilder;
    let temp = tempdir().unwrap();
    let left = build_fs_tree_for_tests(
        temp.path(),
        "left",
        vec![TreeEntry::File {
            path: "bin/tool".to_string(),
            text: "tool\n".to_string(),
            executable: true,
        }],
        sample_install(),
    );
    let right = build_fs_tree_for_tests(
        temp.path(),
        "right",
        vec![TreeEntry::Dir {
            path: "etc".to_string(),
        }],
        sample_install(),
    );
    let mut cx = build_context(&temp.path().join("merge"));

    let result = builder
        .build_typed_for_tests(
            TreeMergeConfig {},
            tree_merge_inputs(&[("left", &left), ("right", &right)]),
            &mut cx,
        )
        .unwrap();

    let src = fs::metadata(fs_tree_root(&left).join("bin/tool")).unwrap();
    let dst = fs::metadata(fs_tree_root(&result).join("bin/tool")).unwrap();
    assert_eq!((src.dev(), src.ino()), (dst.dev(), dst.ino()));
}

#[test]
fn single_file_tree_builds_file_object() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let result = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "hello.txt".to_string(),
                        text: "hello".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();
    assert!(result.staged_path.is_file());
    assert_eq!(fs::read_to_string(&result.staged_path).unwrap(), "hello");
}

#[test]
fn single_nested_file_requires_directory_output() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc/hostname".to_string(),
                        text: "mbuild\n".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("directory output requires install")
    );
}

#[test]
fn single_dir_entry_produces_directory_output() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let result = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::Dir {
                        path: "dev".to_string(),
                    }],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();

    assert!(result.staged_path.is_dir());
    assert!(result.staged_path.join("manifest.jsonl").is_file());
    assert!(fs_tree_root(&result).join("dev").is_dir());
    assert_valid_fs_tree(&result);
}

#[test]
fn materializes_explicit_empty_directories() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let result = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![
                        TreeEntry::Dir {
                            path: "dev".to_string(),
                        },
                        TreeEntry::Dir {
                            path: "proc".to_string(),
                        },
                        TreeEntry::Dir {
                            path: "sys".to_string(),
                        },
                    ],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();

    let root = fs_tree_root(&result);
    assert!(root.join("dev").is_dir());
    assert!(root.join("proc").is_dir());
    assert!(root.join("sys").is_dir());
    assert_eq!(fs::read_dir(root.join("dev")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(root.join("proc")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(root.join("sys")).unwrap().count(), 0);
    assert_valid_fs_tree(&result);
}

#[test]
fn materializes_symlink_entries_with_literal_targets() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let result = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![
                        TreeEntry::Dir {
                            path: "usr/bin".to_string(),
                        },
                        TreeEntry::Symlink {
                            path: "bin".to_string(),
                            target: "usr/bin".to_string(),
                        },
                    ],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();

    let target = fs::read_link(fs_tree_root(&result).join("bin")).unwrap();
    assert_eq!(target, PathBuf::from("usr/bin"));
    assert_valid_fs_tree(&result);
}

#[test]
fn materializes_broken_symlink_entries() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let result = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::Symlink {
                        path: "etc/mtab".to_string(),
                        target: "/proc/self/mounts".to_string(),
                    }],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();

    let target = fs::read_link(fs_tree_root(&result).join("etc/mtab")).unwrap();
    assert_eq!(target, PathBuf::from("/proc/self/mounts"));
    assert_valid_fs_tree(&result);
}

#[test]
fn directory_tree_builds_fs_tree_with_manifest() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let result = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![
                        TreeEntry::Dir {
                            path: "dev".to_string(),
                        },
                        TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        },
                        TreeEntry::File {
                            path: "init".to_string(),
                            text: "#!/bin/sh\n".to_string(),
                            executable: true,
                        },
                        TreeEntry::Symlink {
                            path: "bin".to_string(),
                            target: "usr/bin".to_string(),
                        },
                    ],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();

    assert!(result.staged_path.is_dir());
    let root = fs_tree_root(&result);
    assert!(root.join("dev").is_dir());
    assert_eq!(
        fs::read_to_string(root.join("etc/hostname")).unwrap(),
        "mbuild\n"
    );
    assert_eq!(
        fs::read_link(root.join("bin")).unwrap(),
        PathBuf::from("usr/bin")
    );

    let manifest = fs_tree_manifest(&result);
    assert_eq!(
        manifest.entries(),
        &[
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::symlink_with_hash("bin", 0, 0, "usr/bin", hash_symlink_node(b"usr/bin")),
            FsTreeEntry::directory("dev", 0, 0, 0o755),
            FsTreeEntry::directory("etc", 0, 0, 0o755),
            FsTreeEntry::file_with_hash(
                "etc/hostname",
                0,
                0,
                0o644,
                hash_file_bytes(false, b"mbuild\n")
            ),
            FsTreeEntry::file_with_hash("init", 0, 0, 0o755, hash_file_bytes(true, b"#!/bin/sh\n")),
        ]
    );
    assert_valid_fs_tree(&result);

    #[cfg(unix)]
    {
        let init_mode = fs::metadata(root.join("init"))
            .unwrap()
            .permissions()
            .mode();
        let etc_mode = fs::metadata(root.join("etc")).unwrap().permissions().mode();
        assert_eq!(init_mode & 0o777, 0o755);
        assert_eq!(etc_mode & 0o777, 0o755);
    }
}

#[test]
fn directory_tree_applies_restrictive_directory_modes_after_children() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let result = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "share/info.txt".to_string(),
                        text: "inline\n".to_string(),
                        executable: false,
                    }],
                },
                install: Some(InstallMeta {
                    rules: vec![
                        InstallRule {
                            path: "**".to_string(),
                            attrs: InstallAttrs {
                                uid: Some(0),
                                gid: Some(0),
                                directory_mode: Some(0o755),
                                regular_file_mode: Some(0o644),
                                executable_file_mode: Some(0o755),
                                symlink_mode: None,
                            },
                        },
                        InstallRule {
                            path: "share/**".to_string(),
                            attrs: InstallAttrs {
                                uid: None,
                                gid: None,
                                directory_mode: Some(0o555),
                                regular_file_mode: Some(0o444),
                                executable_file_mode: None,
                                symlink_mode: None,
                            },
                        },
                    ],
                }),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();

    assert_eq!(
        fs::read_to_string(fs_tree_root(&result).join("share/info.txt")).unwrap(),
        "inline\n"
    );
    assert_valid_fs_tree(&result);

    #[cfg(unix)]
    {
        let root = fs_tree_root(&result);
        let share_mode = fs::metadata(root.join("share"))
            .unwrap()
            .permissions()
            .mode();
        let file_mode = fs::metadata(root.join("share/info.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(share_mode & 0o777, 0o555);
        assert_eq!(file_mode & 0o777, 0o444);
    }
}

#[test]
fn tree_fs_object_hash_changes_with_mode_bytes_and_symlink_target() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();

    let mut cx = build_context(temp.path());
    let base = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc/hostname".to_string(),
                        text: "mbuild\n".to_string(),
                        executable: false,
                    }],
                },
                install: Some(install_with_modes(0o755, 0o644)),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();
    let base_hash = fsobj_hash::hash_path(&base.staged_path).unwrap();

    let mut cx = build_context(temp.path());
    let changed_mode = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc/hostname".to_string(),
                        text: "mbuild\n".to_string(),
                        executable: false,
                    }],
                },
                install: Some(install_with_modes(0o700, 0o600)),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();
    assert_ne!(
        base_hash,
        fsobj_hash::hash_path(&changed_mode.staged_path).unwrap()
    );

    let mut cx = build_context(temp.path());
    let changed_bytes = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc/hostname".to_string(),
                        text: "other\n".to_string(),
                        executable: false,
                    }],
                },
                install: Some(install_with_modes(0o755, 0o644)),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();
    assert_ne!(
        base_hash,
        fsobj_hash::hash_path(&changed_bytes.staged_path).unwrap()
    );

    let mut cx = build_context(temp.path());
    let link_a = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::Symlink {
                        path: "bin".to_string(),
                        target: "usr/bin".to_string(),
                    }],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();
    let link_a_hash = fsobj_hash::hash_path(&link_a.staged_path).unwrap();
    let mut cx = build_context(temp.path());
    let link_b = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::Symlink {
                        path: "bin".to_string(),
                        target: "sbin".to_string(),
                    }],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap();
    assert_ne!(
        link_a_hash,
        fsobj_hash::hash_path(&link_b.staged_path).unwrap()
    );
}

#[test]
fn file_output_rejects_install_metadata() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "hello.txt".to_string(),
                        text: "hello".to_string(),
                        executable: false,
                    }],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("file output must not specify install")
    );
}

#[test]
fn directory_output_requires_install_metadata() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc/hostname".to_string(),
                        text: "mbuild\n".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("directory output requires install")
    );
}

#[test]
fn directory_output_rejects_empty_install_rules() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::Dir {
                        path: "dev".to_string(),
                    }],
                },
                install: Some(InstallMeta { rules: vec![] }),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("install.rules must contain at least one rule")
    );
}

#[test]
fn directory_manifest_preserves_non_root_owner_attrs() {
    let entries = normalize_entries(vec![TreeEntry::File {
        path: "etc/hostname".to_string(),
        text: "mbuild\n".to_string(),
        executable: false,
    }])
    .unwrap();
    let rules = compile_install_rules(&install_with_attrs(42, 43, 0o755, 0o644).rules).unwrap();

    let manifest = fs_tree_manifest_for_entries(&entries, &rules).unwrap();

    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::directory("etc", 42, 43, 0o755))
    );
    assert!(manifest.entries().contains(&FsTreeEntry::file_with_hash(
        "etc/hostname",
        42,
        43,
        0o644,
        hash_file_bytes(false, b"mbuild\n")
    )));
}

#[test]
fn partial_ownerless_overrides_inherit_non_root_owner_attrs() {
    let entries = normalize_entries(vec![TreeEntry::File {
        path: "etc/hostname".to_string(),
        text: "mbuild\n".to_string(),
        executable: false,
    }])
    .unwrap();
    let install = InstallMeta {
        rules: vec![
            InstallRule {
                path: "**".to_string(),
                attrs: InstallAttrs {
                    uid: Some(42),
                    gid: Some(43),
                    directory_mode: Some(0o755),
                    regular_file_mode: Some(0o644),
                    executable_file_mode: Some(0o755),
                    symlink_mode: None,
                },
            },
            InstallRule {
                path: "etc/**".to_string(),
                attrs: InstallAttrs {
                    uid: None,
                    gid: None,
                    directory_mode: Some(0o700),
                    regular_file_mode: Some(0o600),
                    executable_file_mode: None,
                    symlink_mode: None,
                },
            },
        ],
    };
    let rules = compile_install_rules(&install.rules).unwrap();

    let manifest = fs_tree_manifest_for_entries(&entries, &rules).unwrap();

    assert!(
        manifest
            .entries()
            .contains(&FsTreeEntry::directory("etc", 42, 43, 0o700))
    );
    assert!(manifest.entries().contains(&FsTreeEntry::file_with_hash(
        "etc/hostname",
        42,
        43,
        0o600,
        hash_file_bytes(false, b"mbuild\n")
    )));
}

#[test]
fn directory_output_rejects_uncovered_paths_and_missing_attrs() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc/hostname".to_string(),
                        text: "mbuild\n".to_string(),
                        executable: false,
                    }],
                },
                install: Some(InstallMeta {
                    rules: vec![InstallRule {
                        path: "bin/**".to_string(),
                        attrs: InstallAttrs {
                            uid: Some(0),
                            gid: Some(0),
                            directory_mode: Some(0o755),
                            regular_file_mode: Some(0o644),
                            executable_file_mode: Some(0o755),
                            symlink_mode: None,
                        },
                    }],
                }),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not covered by any install rule")
    );

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc/hostname".to_string(),
                        text: "mbuild\n".to_string(),
                        executable: false,
                    }],
                },
                install: Some(InstallMeta {
                    rules: vec![InstallRule {
                        path: "**".to_string(),
                        attrs: InstallAttrs {
                            uid: Some(0),
                            gid: None,
                            directory_mode: Some(0o755),
                            regular_file_mode: Some(0o644),
                            executable_file_mode: Some(0o755),
                            symlink_mode: None,
                        },
                    }],
                }),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();
    assert!(error.to_string().contains("missing resolved gid"));
}

#[test]
fn tree_builder_rejects_non_empty_inputs() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());
    let mut inputs = BuilderInputs::empty();
    inputs.insert(
        "unexpected",
        BuilderInputObject {
            path: std::path::PathBuf::from("/tmp/unexpected"),
        },
    );

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "hello.txt".to_string(),
                        text: "hello".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            inputs,
            &mut cx,
        )
        .unwrap_err();

    assert!(matches!(error, BuilderError::ExecutionFailed(_)));
}

#[test]
fn rejects_invalid_and_conflicting_paths() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![
                        TreeEntry::File {
                            path: "etc".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        },
                        TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        },
                    ],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("conflicts with descendant path 'etc/hostname'")
    );

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "../escape".to_string(),
                        text: "bad".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(error.to_string().contains("must not contain '..'"));

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![
                        TreeEntry::Dir {
                            path: "etc".to_string(),
                        },
                        TreeEntry::File {
                            path: "etc".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        },
                    ],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("duplicate tree entry path 'etc'")
    );

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "/etc/hostname".to_string(),
                        text: "bad".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(error.to_string().contains("must be relative"));

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "./etc/hostname".to_string(),
                        text: "bad".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(error.to_string().contains("must not contain '.'"));

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc\\hostname".to_string(),
                        text: "bad".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(error.to_string().contains("must use '/' separators"));

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![TreeEntry::File {
                        path: "etc//hostname".to_string(),
                        text: "bad".to_string(),
                        executable: false,
                    }],
                },
                install: None,
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("must not contain empty segments")
    );

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![
                        TreeEntry::Symlink {
                            path: "bin".to_string(),
                            target: "usr/bin".to_string(),
                        },
                        TreeEntry::File {
                            path: "bin/tool".to_string(),
                            text: "bad".to_string(),
                            executable: false,
                        },
                    ],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("symlink entry 'bin' conflicts with descendant path 'bin/tool'")
    );

    let error = builder
        .build_typed_for_tests(
            TreeConfig {
                tree: TreePayload {
                    entries: vec![
                        TreeEntry::Symlink {
                            path: "bin".to_string(),
                            target: "".to_string(),
                        },
                        TreeEntry::File {
                            path: "etc/hostname".to_string(),
                            text: "mbuild\n".to_string(),
                            executable: false,
                        },
                    ],
                },
                install: Some(sample_install()),
            },
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("symlink target must not be empty")
    );
}

#[test]
fn build_erased_rejects_unknown_config_field() {
    let builder = TreeBuilder;
    let temp = tempdir().unwrap();
    let mut cx = build_context(temp.path());

    let error = builder
        .build_erased(
            serde_json::json!({
                "tree": {
                    "entries": [
                        {
                            "type": "file",
                            "path": "hello.txt",
                            "text": "hello",
                            "executable": false
                        }
                    ]
                },
                "extra": true
            }),
            BuilderInputs::empty(),
            &mut cx,
        )
        .unwrap_err();

    assert!(matches!(error, BuilderError::InvalidRecipe(_)));
}
