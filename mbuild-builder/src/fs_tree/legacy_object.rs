use super::archive::archive_sources;
use crate::BuildContext;
use fsobj_hash::{EntryKind, ObjectHash};
#[cfg(test)]
use mbuild_core::FsTreeOwnerMap;
use mbuild_core::{
    BuildLogLevel, BuilderError, ComposedFsTree, ComposedFsTreeEntry, FsTreeComposeInput,
    FsTreeEntry, FsTreeManifest, compose_fs_trees, load_fs_tree_object,
};
use mbuild_runtime::{FsTreeArchiveInput, FsTreeMaterializeReport};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
#[cfg(all(test, unix))]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub(super) struct IndexedTreeMergeInput {
    pub(super) compose: FsTreeComposeInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InputLeafIdentity {
    kind: EntryKind,
    node_hash: ObjectHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SeenInputLeaf {
    entry: FsTreeEntry,
    identity: InputLeafIdentity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TreeMergeStageStats {
    directory_count: usize,
    file_count: usize,
    hardlinked_file_count: usize,
    copied_file_count: usize,
    symlink_count: usize,
    directory_ms: u128,
    file_validate_ms: u128,
    hardlink_ms: u128,
    copy_ms: u128,
    symlink_ms: u128,
}

impl From<FsTreeMaterializeReport> for TreeMergeStageStats {
    fn from(report: FsTreeMaterializeReport) -> Self {
        Self {
            directory_count: report.directory_count,
            file_count: report.file_count,
            hardlinked_file_count: report.hardlinked_file_count,
            copied_file_count: 0,
            symlink_count: report.symlink_count,
            directory_ms: report.directory_ms,
            file_validate_ms: 0,
            hardlink_ms: report.hardlink_ms,
            copy_ms: 0,
            symlink_ms: report.symlink_ms,
        }
    }
}

pub(super) trait OwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        object_dir: &Path,
        manifest: &FsTreeManifest,
        temp_dir: &Path,
    ) -> Result<(), BuilderError>;
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RuntimeOwnershipMaterializer;

impl OwnershipMaterializer for RuntimeOwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        _object_dir: &Path,
        manifest: &FsTreeManifest,
        temp_dir: &Path,
    ) -> Result<(), BuilderError> {
        mbuild_runtime::apply_ownership_batch(root_dir, manifest, temp_dir)
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;
        Ok(())
    }
}

pub(super) trait FsTreeObjectMaterializer {
    fn materialize_from_sources(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_object_dir: &Path,
        workspace: &Path,
    ) -> Result<FsTreeMaterializeReport, BuilderError>;
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RuntimeFsTreeObjectMaterializer;

impl FsTreeObjectMaterializer for RuntimeFsTreeObjectMaterializer {
    fn materialize_from_sources(
        &self,
        inputs: &[FsTreeComposeInput],
        composed: &ComposedFsTree,
        output_object_dir: &Path,
        workspace: &Path,
    ) -> Result<FsTreeMaterializeReport, BuilderError> {
        let archive_inputs = inputs
            .iter()
            .map(|input| FsTreeArchiveInput {
                root_dir: input.root_dir.clone(),
            })
            .collect::<Vec<_>>();
        let sources = archive_sources("materialize", inputs, composed)?;
        mbuild_runtime::materialize_fs_tree_from_sources_in_ownership_namespace(
            &archive_inputs,
            composed.manifest(),
            &sources,
            output_object_dir,
            workspace,
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
    }
}

pub(super) fn compose_rootfs_inputs_allowing_identical_leaf_overlap(
    builder_name: &str,
    inputs: &[IndexedTreeMergeInput],
) -> Result<ComposedFsTree, BuilderError> {
    let mut seen_leaves = BTreeMap::<String, SeenInputLeaf>::new();
    let mut compose_inputs = Vec::with_capacity(inputs.len());

    for input in inputs {
        let mut entries = Vec::new();
        for entry in input.compose.manifest.entries() {
            if is_fs_tree_leaf(entry) {
                let identity = input_leaf_identity(builder_name, entry)?;
                if let Some(seen) = seen_leaves.get(entry.path()) {
                    ensure_identical_leaf_overlap(entry, identity, seen)?;
                    continue;
                }
                seen_leaves.insert(
                    entry.path().to_string(),
                    SeenInputLeaf {
                        entry: entry.clone(),
                        identity,
                    },
                );
            }
            entries.push(entry.clone());
        }

        compose_inputs.push(FsTreeComposeInput {
            manifest: FsTreeManifest::from_entries(entries).map_err(map_fs_tree_error)?,
            root_dir: input.compose.root_dir.clone(),
        });
    }

    compose_fs_trees(&compose_inputs).map_err(map_fs_tree_error)
}

pub(super) fn input_leaf_identity(
    builder_name: &str,
    entry: &FsTreeEntry,
) -> Result<InputLeafIdentity, BuilderError> {
    let expected_kind = leaf_entry_kind_for_manifest_entry(entry).ok_or_else(|| {
        BuilderError::ExecutionFailed(format!(
            "fs-tree path '{}' is not a file or symlink",
            entry.path()
        ))
    })?;
    let Some(node_hash) = entry.leaf_hash() else {
        return Err(BuilderError::ExecutionFailed(format!(
            "{builder_name} input manifest leaf '{}' is missing hash",
            entry.path()
        )));
    };

    Ok(InputLeafIdentity {
        kind: expected_kind,
        node_hash,
    })
}

pub(super) fn ensure_identical_leaf_overlap(
    entry: &FsTreeEntry,
    identity: InputLeafIdentity,
    seen: &SeenInputLeaf,
) -> Result<(), BuilderError> {
    if seen.entry == *entry && seen.identity == identity {
        return Ok(());
    }

    let reason = if leaf_entry_kind(&seen.entry) != leaf_entry_kind(entry) {
        format!(
            "{} vs {}",
            leaf_entry_kind(&seen.entry),
            leaf_entry_kind(entry)
        )
    } else if seen.entry != *entry {
        "metadata differs".to_string()
    } else {
        "leaf hash differs".to_string()
    };
    Err(BuilderError::ExecutionFailed(format!(
        "conflicting fs-tree entries at '{}': duplicate leaf entries differ ({reason})",
        entry.path()
    )))
}

pub(super) fn is_fs_tree_leaf(entry: &FsTreeEntry) -> bool {
    matches!(
        entry,
        FsTreeEntry::File { .. } | FsTreeEntry::Symlink { .. }
    )
}

pub(super) fn leaf_entry_kind_for_manifest_entry(entry: &FsTreeEntry) -> Option<EntryKind> {
    match entry {
        FsTreeEntry::File { .. } => Some(EntryKind::File),
        FsTreeEntry::Symlink { .. } => Some(EntryKind::Symlink),
        FsTreeEntry::Directory { .. } => None,
    }
}

pub(super) fn leaf_entry_kind(entry: &FsTreeEntry) -> &'static str {
    match entry {
        FsTreeEntry::File { .. } => "file",
        FsTreeEntry::Symlink { .. } => "symlink",
        FsTreeEntry::Directory { .. } => "directory",
    }
}

pub(super) fn load_fs_tree_compose_input(
    object_path: &Path,
) -> Result<FsTreeComposeInput, BuilderError> {
    let loaded = load_fs_tree_object(object_path).map_err(map_fs_tree_error)?;
    Ok(FsTreeComposeInput {
        manifest: loaded.manifest,
        root_dir: loaded.paths.root_dir,
    })
}

pub(super) fn materialize_composed_tree_output(
    label: &str,
    output_path: &Path,
    inputs: &[FsTreeComposeInput],
    composed: &ComposedFsTree,
    temp_dir: &Path,
    materializer: &impl FsTreeObjectMaterializer,
    cx: &mut BuildContext,
) -> Result<ObjectHash, BuilderError> {
    validate_composed_entry_sources(label, composed)?;
    let report = materializer.materialize_from_sources(inputs, composed, output_path, temp_dir)?;
    let stats = TreeMergeStageStats::from(report.clone());

    cx.log_event_with_details(
        BuildLogLevel::Info,
        "stage-done",
        format!(
            "materialized {label} with {} entries",
            composed.manifest().entries().len()
        ),
        None,
        None,
        tree_merge_stage_details(composed.manifest().entries().len(), &stats),
    );
    log_tree_merge_ownership_events(cx, report.ownership_ms);

    let hash_start = Instant::now();
    let object_hash = hash_tree_output_from_manifest(composed)?;
    log_tree_merge_hash_event(cx, object_hash, elapsed_ms(hash_start), 0);
    Ok(object_hash)
}

pub(super) fn validate_composed_entry_sources(
    label: &str,
    composed: &ComposedFsTree,
) -> Result<(), BuilderError> {
    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(composed.entries())
    {
        match (manifest_entry, composed_entry) {
            (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory)
            | (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { .. })
            | (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {}
            _ => {
                return Err(BuilderError::ExecutionFailed(format!(
                    "{label} entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn hash_tree_output_from_manifest(
    composed: &ComposedFsTree,
) -> Result<ObjectHash, BuilderError> {
    mbuild_core::hash_fs_tree_object_from_manifest(composed.manifest()).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to hash fs-tree object from manifest: {error}"
        ))
    })
}

pub(super) fn log_tree_merge_ownership_events(cx: &mut BuildContext, host_duration_ms: u128) {
    let mut ownership_details = Map::new();
    ownership_details.insert("duration_ms".to_string(), json_u128(host_duration_ms));
    ownership_details.insert("host_duration_ms".to_string(), json_u128(host_duration_ms));
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "ownership-done",
        format!("materialized ownership in {host_duration_ms} ms"),
        None,
        None,
        ownership_details,
    );
}

pub(super) fn log_tree_merge_hash_event(
    cx: &mut BuildContext,
    object_hash: ObjectHash,
    hash_ms: u128,
    manifest_serialize_ms: u128,
) {
    let mut hash_details = Map::new();
    hash_details.insert("duration_ms".to_string(), json_u128(hash_ms));
    hash_details.insert(
        "manifest_serialize_ms".to_string(),
        json_u128(manifest_serialize_ms),
    );
    cx.log_event_with_details(
        BuildLogLevel::Info,
        "hash-done",
        format!("hashed merged fs-tree object in {hash_ms} ms"),
        Some(object_hash),
        None,
        hash_details,
    );
}

pub(super) fn tree_merge_compose_details(
    input_count: usize,
    entry_count: usize,
    duration_ms: u128,
) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("duration_ms".to_string(), json_u128(duration_ms));
    details.insert("input_count".to_string(), json_usize(input_count));
    details.insert("entry_count".to_string(), json_usize(entry_count));
    details
}

pub(super) fn tree_subset_compose_details(
    pattern_count: usize,
    entry_count: usize,
    duration_ms: u128,
) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("duration_ms".to_string(), json_u128(duration_ms));
    details.insert("pattern_count".to_string(), json_usize(pattern_count));
    details.insert("entry_count".to_string(), json_usize(entry_count));
    details
}

pub(super) fn tree_merge_stage_details(
    entry_count: usize,
    stats: &TreeMergeStageStats,
) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert(
        "duration_ms".to_string(),
        json_u128(
            stats.directory_ms
                + stats.file_validate_ms
                + stats.hardlink_ms
                + stats.copy_ms
                + stats.symlink_ms,
        ),
    );
    details.insert("entry_count".to_string(), json_usize(entry_count));
    details.insert(
        "directory_count".to_string(),
        json_usize(stats.directory_count),
    );
    details.insert("file_count".to_string(), json_usize(stats.file_count));
    details.insert(
        "hardlinked_file_count".to_string(),
        json_usize(stats.hardlinked_file_count),
    );
    details.insert(
        "copied_file_count".to_string(),
        json_usize(stats.copied_file_count),
    );
    details.insert("symlink_count".to_string(), json_usize(stats.symlink_count));
    details.insert("directory_ms".to_string(), json_u128(stats.directory_ms));
    details.insert(
        "file_validate_ms".to_string(),
        json_u128(stats.file_validate_ms),
    );
    details.insert("hardlink_ms".to_string(), json_u128(stats.hardlink_ms));
    details.insert("copy_ms".to_string(), json_u128(stats.copy_ms));
    details.insert("symlink_ms".to_string(), json_u128(stats.symlink_ms));
    details
}

pub(super) fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

pub(super) fn current_epoch_nanos() -> Result<u128, BuilderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!("system time before UNIX_EPOCH: {error}"))
        })
}

pub(super) fn json_usize(value: usize) -> Value {
    Value::from(value as u64)
}

pub(super) fn json_u128(value: u128) -> Value {
    Value::from(value.min(u64::MAX as u128) as u64)
}

#[cfg(all(test, unix))]
pub(super) fn validate_tree_merge_file_attrs(
    source: &Path,
    manifest_entry: &FsTreeEntry,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), BuilderError> {
    let FsTreeEntry::File {
        path,
        uid,
        gid,
        mode,
        ..
    } = manifest_entry
    else {
        return Err(BuilderError::ExecutionFailed(format!(
            "expected file manifest entry for '{}'",
            source.display()
        )));
    };

    let metadata = fs::symlink_metadata(source).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to inspect merged fs-tree source file '{}': {error}",
            source.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source '{}' for '{}' must be a file",
            source.display(),
            path
        )));
    }

    let expected_uid = owner_map.physical_uid(*uid).map_err(map_fs_tree_error)?;
    let expected_gid = owner_map.physical_gid(*gid).map_err(map_fs_tree_error)?;
    if metadata.uid() != expected_uid || metadata.gid() != expected_gid {
        return Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source file '{}' for '{}' has owner {}:{}, expected {}:{}",
            source.display(),
            path,
            metadata.uid(),
            metadata.gid(),
            expected_uid,
            expected_gid
        )));
    }

    let actual_mode = metadata.permissions().mode() & 0o7777;
    if actual_mode != *mode {
        return Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source file '{}' for '{}' has mode {:o}, expected {:o}",
            source.display(),
            path,
            actual_mode,
            mode
        )));
    }

    Ok(())
}

#[cfg(all(test, not(unix)))]
pub(super) fn validate_tree_merge_file_attrs(
    source: &Path,
    manifest_entry: &FsTreeEntry,
    _owner_map: &impl FsTreeOwnerMap,
) -> Result<(), BuilderError> {
    if source.is_file() {
        Ok(())
    } else {
        Err(BuilderError::ExecutionFailed(format!(
            "merged fs-tree source '{}' for '{}' must be a file",
            source.display(),
            manifest_entry.path()
        )))
    }
}

pub(super) fn set_file_mode(path: &Path, executable: bool) -> Result<(), BuilderError> {
    #[cfg(unix)]
    {
        let mode = if executable { 0o755 } else { 0o644 };
        set_mode(path, mode)?;
    }
    Ok(())
}

pub(super) fn set_mode(path: &Path, mode: u32) -> Result<(), BuilderError> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to set permissions on staged path '{}': {error}",
                path.display()
            ))
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = mode;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn create_symlink(target: &str, path: &Path) -> Result<(), BuilderError> {
    std::os::unix::fs::symlink(target, path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create staged symlink '{}' -> '{}': {error}",
            path.display(),
            target
        ))
    })
}

#[cfg(not(unix))]
pub(super) fn create_symlink(_target: &str, _path: &Path) -> Result<(), BuilderError> {
    Err(BuilderError::ExecutionFailed(
        "Tree symlink entries are only supported on unix platforms".to_string(),
    ))
}

pub(super) fn map_fs_tree_error(error: impl std::fmt::Display) -> BuilderError {
    BuilderError::ExecutionFailed(error.to_string())
}
