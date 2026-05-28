use crate::{
    FsTreeEntry, FsTreeManifest, FsTreeManifestError, FsTreeObjectError, FsTreeObjectPaths,
    FsTreeOwnerMap, ValidatedFsTreeObject, create_fs_tree_staging_dir, validate_fs_tree_object,
};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum FsTreeComposeError {
    Invalid(String),
    Conflict(String),
    Io(String),
    Manifest(FsTreeManifestError),
    Object(FsTreeObjectError),
}

impl fmt::Display for FsTreeComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Conflict(message) | Self::Io(message) => {
                f.write_str(message)
            }
            Self::Manifest(error) => write!(f, "{error}"),
            Self::Object(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FsTreeComposeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Object(error) => Some(error),
            Self::Invalid(_) | Self::Conflict(_) | Self::Io(_) => None,
        }
    }
}

impl From<FsTreeManifestError> for FsTreeComposeError {
    fn from(error: FsTreeManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<FsTreeObjectError> for FsTreeComposeError {
    fn from(error: FsTreeObjectError) -> Self {
        Self::Object(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeComposeInput {
    pub manifest: FsTreeManifest,
    pub root_dir: PathBuf,
}

impl From<ValidatedFsTreeObject> for FsTreeComposeInput {
    fn from(object: ValidatedFsTreeObject) -> Self {
        Self {
            manifest: object.manifest,
            root_dir: object.paths.root_dir,
        }
    }
}

impl From<&ValidatedFsTreeObject> for FsTreeComposeInput {
    fn from(object: &ValidatedFsTreeObject) -> Self {
        Self {
            manifest: object.manifest.clone(),
            root_dir: object.paths.root_dir.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedFsTree {
    pub manifest: FsTreeManifest,
    pub entries: Vec<ComposedFsTreeEntry>,
}

impl ComposedFsTree {
    pub fn manifest(&self) -> &FsTreeManifest {
        &self.manifest
    }

    pub fn entries(&self) -> &[ComposedFsTreeEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposedFsTreeEntry {
    Directory,
    File { source_path: PathBuf },
    Symlink { source_path: PathBuf },
}

pub trait FsTreeOwnerApplier {
    fn apply_directory_owner(
        &self,
        path: &Path,
        physical_uid: u32,
        physical_gid: u32,
    ) -> Result<(), FsTreeComposeError>;

    fn apply_file_owner(
        &self,
        path: &Path,
        physical_uid: u32,
        physical_gid: u32,
    ) -> Result<(), FsTreeComposeError>;

    fn apply_symlink_owner(
        &self,
        path: &Path,
        physical_uid: u32,
        physical_gid: u32,
    ) -> Result<(), FsTreeComposeError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CurrentOwnerOnlyFsTreeOwnerApplier;

impl FsTreeOwnerApplier for CurrentOwnerOnlyFsTreeOwnerApplier {
    fn apply_directory_owner(
        &self,
        path: &Path,
        physical_uid: u32,
        physical_gid: u32,
    ) -> Result<(), FsTreeComposeError> {
        require_current_owner(path, "directory", physical_uid, physical_gid)
    }

    fn apply_file_owner(
        &self,
        path: &Path,
        physical_uid: u32,
        physical_gid: u32,
    ) -> Result<(), FsTreeComposeError> {
        require_current_owner(path, "file", physical_uid, physical_gid)
    }

    fn apply_symlink_owner(
        &self,
        path: &Path,
        physical_uid: u32,
        physical_gid: u32,
    ) -> Result<(), FsTreeComposeError> {
        require_current_owner(path, "symlink", physical_uid, physical_gid)
    }
}

pub fn compose_fs_trees(
    inputs: &[FsTreeComposeInput],
) -> Result<ComposedFsTree, FsTreeComposeError> {
    if inputs.is_empty() {
        return Err(FsTreeComposeError::Invalid(
            "fs-tree composition requires at least one input".to_string(),
        ));
    }

    let mut by_path = BTreeMap::<String, (FsTreeEntry, ComposedFsTreeEntry)>::new();
    for input in inputs {
        for entry in input.manifest.entries() {
            insert_entry(&mut by_path, input, entry)?;
        }
    }

    let entries = by_path
        .values()
        .map(|(entry, _)| entry.clone())
        .collect::<Vec<_>>();
    let manifest = FsTreeManifest::from_entries(entries)?;
    let entries = manifest
        .entries()
        .iter()
        .map(|entry| {
            by_path
                .get(entry.path())
                .expect("manifest entry came from by_path")
                .1
                .clone()
        })
        .collect();

    Ok(ComposedFsTree { manifest, entries })
}

pub fn materialize_composed_fs_tree(
    output_dir: &Path,
    composed: &ComposedFsTree,
    owner_map: &impl FsTreeOwnerMap,
    owner_applier: &impl FsTreeOwnerApplier,
) -> Result<FsTreeObjectPaths, FsTreeComposeError> {
    materialize_composed_fs_tree_with_linker(
        output_dir,
        composed,
        owner_map,
        owner_applier,
        &StdFsTreeLinker,
    )
}

fn insert_entry(
    by_path: &mut BTreeMap<String, (FsTreeEntry, ComposedFsTreeEntry)>,
    input: &FsTreeComposeInput,
    entry: &FsTreeEntry,
) -> Result<(), FsTreeComposeError> {
    let path = entry.path();
    if let Some((existing_entry, _)) = by_path.get(path) {
        if directories_match(existing_entry, entry) {
            return Ok(());
        }

        return Err(FsTreeComposeError::Conflict(format!(
            "conflicting fs-tree entries at '{}': {} vs {}",
            path,
            entry_kind(existing_entry),
            entry_kind(entry)
        )));
    }

    reject_existing_non_directory_parent(by_path, path)?;
    reject_descendant_under_new_leaf(by_path, entry)?;

    let composed_entry = match entry {
        FsTreeEntry::Directory { .. } => ComposedFsTreeEntry::Directory,
        FsTreeEntry::File { path, .. } => ComposedFsTreeEntry::File {
            source_path: input.root_dir.join(path),
        },
        FsTreeEntry::Symlink { path, .. } => ComposedFsTreeEntry::Symlink {
            source_path: input.root_dir.join(path),
        },
    };
    by_path.insert(path.to_string(), (entry.clone(), composed_entry));
    Ok(())
}

fn directories_match(left: &FsTreeEntry, right: &FsTreeEntry) -> bool {
    matches!(
        (left, right),
        (
            FsTreeEntry::Directory {
                path: left_path,
                uid: left_uid,
                gid: left_gid,
                mode: left_mode,
            },
            FsTreeEntry::Directory {
                path: right_path,
                uid: right_uid,
                gid: right_gid,
                mode: right_mode,
            },
        ) if left_path == right_path
            && left_uid == right_uid
            && left_gid == right_gid
            && left_mode == right_mode
    )
}

fn reject_existing_non_directory_parent(
    by_path: &BTreeMap<String, (FsTreeEntry, ComposedFsTreeEntry)>,
    path: &str,
) -> Result<(), FsTreeComposeError> {
    let mut remainder = path;
    while let Some((parent, _)) = remainder.rsplit_once('/') {
        if let Some((entry, _)) = by_path.get(parent)
            && !matches!(entry, FsTreeEntry::Directory { .. })
        {
            return Err(FsTreeComposeError::Conflict(format!(
                "fs-tree path '{}' is under non-directory '{}'",
                path, parent
            )));
        }
        remainder = parent;
    }

    Ok(())
}

fn reject_descendant_under_new_leaf(
    by_path: &BTreeMap<String, (FsTreeEntry, ComposedFsTreeEntry)>,
    entry: &FsTreeEntry,
) -> Result<(), FsTreeComposeError> {
    if matches!(entry, FsTreeEntry::Directory { .. }) {
        return Ok(());
    }

    let path = entry.path();
    let prefix = format!("{path}/");
    if let Some((descendant, _)) = by_path.range(prefix.clone()..).next()
        && descendant.starts_with(&prefix)
    {
        return Err(FsTreeComposeError::Conflict(format!(
            "fs-tree non-directory '{}' conflicts with descendant '{}'",
            path, descendant
        )));
    }

    Ok(())
}

fn entry_kind(entry: &FsTreeEntry) -> &'static str {
    match entry {
        FsTreeEntry::Directory { .. } => "directory",
        FsTreeEntry::File { .. } => "file",
        FsTreeEntry::Symlink { .. } => "symlink",
    }
}

fn materialize_composed_fs_tree_with_linker(
    output_dir: &Path,
    composed: &ComposedFsTree,
    owner_map: &impl FsTreeOwnerMap,
    owner_applier: &impl FsTreeOwnerApplier,
    linker: &impl FsTreeLinker,
) -> Result<FsTreeObjectPaths, FsTreeComposeError> {
    #[cfg(not(unix))]
    {
        let _ = output_dir;
        let _ = composed;
        let _ = owner_map;
        let _ = owner_applier;
        let _ = linker;
        return Err(FsTreeComposeError::Invalid(
            "fs-tree composition materialization is only supported on unix hosts".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        let existed_before = output_dir.exists() || output_dir.is_symlink();
        let result = materialize_composed_fs_tree_inner(
            output_dir,
            composed,
            owner_map,
            owner_applier,
            linker,
        );
        if result.is_err() && !existed_before && (output_dir.exists() || output_dir.is_symlink()) {
            let _ = fs::remove_dir_all(output_dir);
        }
        result
    }
}

#[cfg(unix)]
fn materialize_composed_fs_tree_inner(
    output_dir: &Path,
    composed: &ComposedFsTree,
    owner_map: &impl FsTreeOwnerMap,
    owner_applier: &impl FsTreeOwnerApplier,
    linker: &impl FsTreeLinker,
) -> Result<FsTreeObjectPaths, FsTreeComposeError> {
    validate_composed_shape(composed)?;
    let paths = create_fs_tree_staging_dir(output_dir, composed.manifest())?;

    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(&composed.entries)
    {
        match (manifest_entry, composed_entry) {
            (
                FsTreeEntry::Directory {
                    path,
                    uid,
                    gid,
                    mode,
                },
                ComposedFsTreeEntry::Directory,
            ) => {
                let dst = paths.root_dir.join(path);
                if !path.is_empty() {
                    fs::create_dir(&dst).map_err(|error| {
                        FsTreeComposeError::Io(format!(
                            "failed to create fs-tree directory '{}': {error}",
                            dst.display()
                        ))
                    })?;
                }
                apply_directory_owner(&dst, *uid, *gid, owner_map, owner_applier)?;
                chmod(&dst, *mode)?;
            }
            (
                FsTreeEntry::File {
                    path,
                    uid,
                    gid,
                    mode,
                    ..
                },
                ComposedFsTreeEntry::File { source_path },
            ) => {
                let dst = paths.root_dir.join(path);
                let file_attrs = FileMaterializationAttrs {
                    logical_uid: *uid,
                    logical_gid: *gid,
                    mode: *mode,
                };
                link_or_copy_file(
                    source_path,
                    &dst,
                    file_attrs,
                    owner_map,
                    owner_applier,
                    linker,
                )?;
            }
            (
                FsTreeEntry::Symlink {
                    path,
                    uid,
                    gid,
                    target,
                    ..
                },
                ComposedFsTreeEntry::Symlink { source_path },
            ) => {
                let dst = paths.root_dir.join(path);
                let source_target = fs::read_link(source_path).map_err(|error| {
                    FsTreeComposeError::Io(format!(
                        "failed to read fs-tree source symlink '{}': {error}",
                        source_path.display()
                    ))
                })?;
                let source_target = source_target.to_str().ok_or_else(|| {
                    FsTreeComposeError::Invalid(format!(
                        "fs-tree source symlink '{}' target is not UTF-8",
                        source_path.display()
                    ))
                })?;
                if source_target != target {
                    return Err(FsTreeComposeError::Invalid(format!(
                        "fs-tree source symlink '{}' target differs from manifest",
                        source_path.display()
                    )));
                }
                symlink(target.as_str(), &dst).map_err(|error| {
                    FsTreeComposeError::Io(format!(
                        "failed to create fs-tree symlink '{}': {error}",
                        dst.display()
                    ))
                })?;
                apply_symlink_owner(&dst, *uid, *gid, owner_map, owner_applier)?;
            }
            _ => {
                return Err(FsTreeComposeError::Invalid(format!(
                    "composed fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                )));
            }
        }
    }

    validate_fs_tree_object(output_dir, owner_map)?;
    Ok(paths)
}

fn validate_composed_shape(composed: &ComposedFsTree) -> Result<(), FsTreeComposeError> {
    if composed.manifest().entries().len() != composed.entries().len() {
        return Err(FsTreeComposeError::Invalid(format!(
            "composed fs-tree has {} manifest entries but {} materialization entries",
            composed.manifest().entries().len(),
            composed.entries().len()
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct FileMaterializationAttrs {
    logical_uid: u32,
    logical_gid: u32,
    mode: u32,
}

#[cfg(unix)]
fn link_or_copy_file(
    source: &Path,
    dst: &Path,
    attrs: FileMaterializationAttrs,
    owner_map: &impl FsTreeOwnerMap,
    owner_applier: &impl FsTreeOwnerApplier,
    linker: &impl FsTreeLinker,
) -> Result<(), FsTreeComposeError> {
    match linker.hard_link(source, dst) {
        Ok(()) => Ok(()),
        Err(error) if should_copy_after_link_error(error.kind()) => {
            fs::copy(source, dst).map_err(|copy_error| {
                FsTreeComposeError::Io(format!(
                    "failed to copy fs-tree file '{}' to '{}': {copy_error}",
                    source.display(),
                    dst.display()
                ))
            })?;
            apply_file_owner(
                dst,
                attrs.logical_uid,
                attrs.logical_gid,
                owner_map,
                owner_applier,
            )?;
            chmod(dst, attrs.mode)?;
            Ok(())
        }
        Err(error) => Err(FsTreeComposeError::Io(format!(
            "failed to hardlink fs-tree file '{}' to '{}': {error}",
            source.display(),
            dst.display()
        ))),
    }
}

#[cfg(unix)]
fn should_copy_after_link_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::CrossesDevices
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
            | io::ErrorKind::TooManyLinks
    )
}

#[cfg(unix)]
fn apply_directory_owner(
    path: &Path,
    logical_uid: u32,
    logical_gid: u32,
    owner_map: &impl FsTreeOwnerMap,
    owner_applier: &impl FsTreeOwnerApplier,
) -> Result<(), FsTreeComposeError> {
    let (physical_uid, physical_gid) = map_owner(logical_uid, logical_gid, owner_map)?;
    owner_applier.apply_directory_owner(path, physical_uid, physical_gid)
}

#[cfg(unix)]
fn apply_file_owner(
    path: &Path,
    logical_uid: u32,
    logical_gid: u32,
    owner_map: &impl FsTreeOwnerMap,
    owner_applier: &impl FsTreeOwnerApplier,
) -> Result<(), FsTreeComposeError> {
    let (physical_uid, physical_gid) = map_owner(logical_uid, logical_gid, owner_map)?;
    owner_applier.apply_file_owner(path, physical_uid, physical_gid)
}

#[cfg(unix)]
fn apply_symlink_owner(
    path: &Path,
    logical_uid: u32,
    logical_gid: u32,
    owner_map: &impl FsTreeOwnerMap,
    owner_applier: &impl FsTreeOwnerApplier,
) -> Result<(), FsTreeComposeError> {
    let (physical_uid, physical_gid) = map_owner(logical_uid, logical_gid, owner_map)?;
    owner_applier.apply_symlink_owner(path, physical_uid, physical_gid)
}

#[cfg(unix)]
fn map_owner(
    logical_uid: u32,
    logical_gid: u32,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(u32, u32), FsTreeComposeError> {
    Ok((
        owner_map.physical_uid(logical_uid)?,
        owner_map.physical_gid(logical_gid)?,
    ))
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) -> Result<(), FsTreeComposeError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        FsTreeComposeError::Io(format!(
            "failed to set fs-tree entry mode '{}': {error}",
            path.display()
        ))
    })
}

#[cfg(unix)]
fn require_current_owner(
    path: &Path,
    kind: &str,
    physical_uid: u32,
    physical_gid: u32,
) -> Result<(), FsTreeComposeError> {
    let current_uid = unsafe { libc::geteuid() };
    let current_gid = unsafe { libc::getegid() };
    if physical_uid == current_uid && physical_gid == current_gid {
        return Ok(());
    }

    Err(FsTreeComposeError::Invalid(format!(
        "cannot materialize fs-tree {kind} '{}' with physical owner {}:{} as current owner {}:{}",
        path.display(),
        physical_uid,
        physical_gid,
        current_uid,
        current_gid
    )))
}

#[cfg(not(unix))]
fn require_current_owner(
    path: &Path,
    kind: &str,
    physical_uid: u32,
    physical_gid: u32,
) -> Result<(), FsTreeComposeError> {
    let _ = path;
    let _ = kind;
    let _ = physical_uid;
    let _ = physical_gid;
    Err(FsTreeComposeError::Invalid(
        "current-owner fs-tree materialization is only supported on unix hosts".to_string(),
    ))
}

trait FsTreeLinker {
    fn hard_link(&self, source: &Path, dst: &Path) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy)]
struct StdFsTreeLinker;

impl FsTreeLinker for StdFsTreeLinker {
    fn hard_link(&self, source: &Path, dst: &Path) -> io::Result<()> {
        fs::hard_link(source, dst)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::FsTreeObjectError;
    use fsobj_hash::{hash_file_bytes, hash_symlink_node};
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    #[derive(Debug, Clone, Copy)]
    struct ConstantOwnerMap {
        uid: u32,
        gid: u32,
    }

    impl FsTreeOwnerMap for ConstantOwnerMap {
        fn physical_uid(&self, _logical_uid: u32) -> Result<u32, FsTreeObjectError> {
            Ok(self.uid)
        }

        fn physical_gid(&self, _logical_gid: u32) -> Result<u32, FsTreeObjectError> {
            Ok(self.gid)
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct FailingLinker {
        kind: io::ErrorKind,
    }

    impl FsTreeLinker for FailingLinker {
        fn hard_link(&self, _source: &Path, _dst: &Path) -> io::Result<()> {
            Err(io::Error::from(self.kind))
        }
    }

    fn current_owner() -> ConstantOwnerMap {
        ConstantOwnerMap {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        }
    }

    fn non_current_owner() -> ConstantOwnerMap {
        let owner = current_owner();
        ConstantOwnerMap {
            uid: owner.uid.saturating_add(1),
            gid: owner.gid,
        }
    }

    fn root() -> FsTreeEntry {
        FsTreeEntry::directory("", 0, 0, 0o755)
    }

    fn payload_file(path: &str, mode: u32) -> FsTreeEntry {
        FsTreeEntry::file_with_hash(
            path,
            0,
            0,
            mode,
            hash_file_bytes(mode & 0o111 != 0, b"payload\n"),
        )
    }

    fn target_symlink(path: &str) -> FsTreeEntry {
        FsTreeEntry::symlink_with_hash(path, 0, 0, "tool", hash_symlink_node(b"tool"))
    }

    fn hashed_symlink(path: &str, target: &str) -> FsTreeEntry {
        FsTreeEntry::symlink_with_hash(path, 0, 0, target, hash_symlink_node(target.as_bytes()))
    }

    fn manifest(entries: Vec<FsTreeEntry>) -> FsTreeManifest {
        FsTreeManifest::from_entries(entries).unwrap()
    }

    fn input(base: &Path, name: &str, entries: Vec<FsTreeEntry>) -> FsTreeComposeInput {
        let manifest = manifest(entries);
        let object_dir = base.join(name);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        FsTreeComposeInput {
            manifest,
            root_dir: paths.root_dir,
        }
    }

    fn write_file(path: &Path, bytes: &[u8], mode: u32) {
        let mut file = File::create(path).unwrap();
        file.write_all(bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write_input_file(input: &FsTreeComposeInput, rel: &str, bytes: &[u8], mode: u32) {
        write_file(&input.root_dir.join(rel), bytes, mode);
    }

    fn create_input_dir(input: &FsTreeComposeInput, rel: &str, mode: u32) {
        let path = input.root_dir.join(rel);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn compose_single_input_preserves_manifest_and_sources() {
        let temp = tempdir().unwrap();
        let input = input(
            temp.path(),
            "input",
            vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o755),
                payload_file("bin/tool", 0o755),
                target_symlink("bin/tool-link"),
            ],
        );

        let composed = compose_fs_trees(std::slice::from_ref(&input)).unwrap();

        assert_eq!(composed.manifest, input.manifest);
        assert_eq!(composed.entries.len(), composed.manifest.entries().len());
        assert_eq!(composed.entries[0], ComposedFsTreeEntry::Directory);
        assert_eq!(composed.entries[1], ComposedFsTreeEntry::Directory);
        assert_eq!(
            composed.entries[2],
            ComposedFsTreeEntry::File {
                source_path: input.root_dir.join("bin/tool")
            }
        );
        assert_eq!(
            composed.entries[3],
            ComposedFsTreeEntry::Symlink {
                source_path: input.root_dir.join("bin/tool-link")
            }
        );
    }

    #[test]
    fn matching_directory_overlap_is_allowed() {
        let temp = tempdir().unwrap();
        let left = input(
            temp.path(),
            "left",
            vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o755),
                payload_file("bin/left", 0o644),
            ],
        );
        let right = input(
            temp.path(),
            "right",
            vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o755),
                payload_file("bin/right", 0o644),
            ],
        );

        let composed = compose_fs_trees(&[left, right]).unwrap();

        assert!(
            composed
                .manifest
                .entries()
                .iter()
                .any(|e| e.path() == "bin/left")
        );
        assert!(
            composed
                .manifest
                .entries()
                .iter()
                .any(|e| e.path() == "bin/right")
        );
    }

    #[test]
    fn root_or_directory_attr_mismatch_is_conflict() {
        let temp = tempdir().unwrap();
        let left = input(temp.path(), "left", vec![root()]);
        let bad_root = input(
            temp.path(),
            "bad-root",
            vec![FsTreeEntry::directory("", 0, 0, 0o700)],
        );
        assert!(matches!(
            compose_fs_trees(&[left.clone(), bad_root]),
            Err(FsTreeComposeError::Conflict(_))
        ));

        let left = input(
            temp.path(),
            "left-dir",
            vec![root(), FsTreeEntry::directory("dir", 0, 0, 0o755)],
        );
        let right = input(
            temp.path(),
            "right-dir",
            vec![root(), FsTreeEntry::directory("dir", 0, 0, 0o700)],
        );
        assert!(matches!(
            compose_fs_trees(&[left, right]),
            Err(FsTreeComposeError::Conflict(_))
        ));
    }

    #[test]
    fn duplicate_files_and_symlinks_are_conflicts() {
        let temp = tempdir().unwrap();
        let left_file = input(
            temp.path(),
            "left-file",
            vec![root(), payload_file("x", 0o644)],
        );
        let right_file = input(
            temp.path(),
            "right-file",
            vec![root(), payload_file("x", 0o644)],
        );
        assert!(matches!(
            compose_fs_trees(&[left_file, right_file]),
            Err(FsTreeComposeError::Conflict(_))
        ));

        let left_link = input(
            temp.path(),
            "left-link",
            vec![root(), hashed_symlink("x", "target")],
        );
        let right_link = input(
            temp.path(),
            "right-link",
            vec![root(), hashed_symlink("x", "target")],
        );
        assert!(matches!(
            compose_fs_trees(&[left_link, right_link]),
            Err(FsTreeComposeError::Conflict(_))
        ));
    }

    #[test]
    fn file_directory_and_symlink_kind_conflicts_are_rejected() {
        let temp = tempdir().unwrap();
        let file = input(temp.path(), "file", vec![root(), payload_file("x", 0o644)]);
        let dir = input(
            temp.path(),
            "dir",
            vec![root(), FsTreeEntry::directory("x", 0, 0, 0o755)],
        );
        assert!(matches!(
            compose_fs_trees(&[file, dir]),
            Err(FsTreeComposeError::Conflict(_))
        ));

        let link = input(
            temp.path(),
            "link",
            vec![root(), hashed_symlink("x", "target")],
        );
        let file = input(temp.path(), "file2", vec![root(), payload_file("x", 0o644)]);
        assert!(matches!(
            compose_fs_trees(&[link, file]),
            Err(FsTreeComposeError::Conflict(_))
        ));
    }

    #[test]
    fn parent_child_conflict_across_inputs_is_rejected() {
        let temp = tempdir().unwrap();
        let leaf = input(temp.path(), "leaf", vec![root(), payload_file("a", 0o644)]);
        let child = input(
            temp.path(),
            "child",
            vec![
                root(),
                FsTreeEntry::directory("a", 0, 0, 0o755),
                payload_file("a/b", 0o644),
            ],
        );
        assert!(matches!(
            compose_fs_trees(&[leaf.clone(), child.clone()]),
            Err(FsTreeComposeError::Conflict(_))
        ));
        assert!(matches!(
            compose_fs_trees(&[child, leaf]),
            Err(FsTreeComposeError::Conflict(_))
        ));
    }

    #[test]
    fn materialization_writes_manifest_root_content_and_validates() {
        let temp = tempdir().unwrap();
        let input = input(
            temp.path(),
            "input",
            vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o755),
                payload_file("bin/tool", 0o644),
                target_symlink("bin/tool-link"),
            ],
        );
        create_input_dir(&input, "bin", 0o755);
        write_input_file(&input, "bin/tool", b"payload\n", 0o644);
        symlink("tool", input.root_dir.join("bin/tool-link")).unwrap();
        let composed = compose_fs_trees(std::slice::from_ref(&input)).unwrap();
        let output = temp.path().join("output");
        let owner = current_owner();

        let paths = materialize_composed_fs_tree(
            &output,
            &composed,
            &owner,
            &CurrentOwnerOnlyFsTreeOwnerApplier,
        )
        .unwrap();

        assert_eq!(
            FsTreeManifest::read_canonical(&paths.manifest_path).unwrap(),
            composed.manifest
        );
        assert_eq!(
            fs::read(paths.root_dir.join("bin/tool")).unwrap(),
            b"payload\n"
        );
        assert_eq!(
            fs::read_link(paths.root_dir.join("bin/tool-link")).unwrap(),
            PathBuf::from("tool")
        );
        assert!(validate_fs_tree_object(&output, &owner).is_ok());
    }

    #[test]
    fn materialization_hardlinks_files_when_possible() {
        let temp = tempdir().unwrap();
        let input = input(
            temp.path(),
            "input",
            vec![root(), payload_file("file", 0o644)],
        );
        write_input_file(&input, "file", b"payload\n", 0o644);
        let composed = compose_fs_trees(std::slice::from_ref(&input)).unwrap();
        let owner = current_owner();

        let paths = materialize_composed_fs_tree(
            &temp.path().join("output"),
            &composed,
            &owner,
            &CurrentOwnerOnlyFsTreeOwnerApplier,
        )
        .unwrap();

        let src = fs::metadata(input.root_dir.join("file")).unwrap();
        let dst = fs::metadata(paths.root_dir.join("file")).unwrap();
        assert_eq!((src.dev(), src.ino()), (dst.dev(), dst.ino()));
    }

    #[test]
    fn copy_fallback_restores_mode_and_validates() {
        let temp = tempdir().unwrap();
        let input = input(
            temp.path(),
            "input",
            vec![root(), payload_file("file", 0o755)],
        );
        write_input_file(&input, "file", b"payload\n", 0o644);
        let composed = compose_fs_trees(std::slice::from_ref(&input)).unwrap();
        let owner = current_owner();

        let paths = materialize_composed_fs_tree_with_linker(
            &temp.path().join("output"),
            &composed,
            &owner,
            &CurrentOwnerOnlyFsTreeOwnerApplier,
            &FailingLinker {
                kind: io::ErrorKind::CrossesDevices,
            },
        )
        .unwrap();

        let dst = fs::metadata(paths.root_dir.join("file")).unwrap();
        assert_eq!(fs::read(paths.root_dir.join("file")).unwrap(), b"payload\n");
        assert_eq!(dst.permissions().mode() & 0o7777, 0o755);
        assert!(validate_fs_tree_object(&paths.object_dir, &owner).is_ok());
    }

    #[test]
    fn materialization_rejects_non_current_owner_and_removes_partial_output() {
        let temp = tempdir().unwrap();
        let input = input(
            temp.path(),
            "input",
            vec![root(), payload_file("file", 0o644)],
        );
        write_input_file(&input, "file", b"payload\n", 0o644);
        let composed = compose_fs_trees(std::slice::from_ref(&input)).unwrap();
        let output = temp.path().join("output");

        let error = materialize_composed_fs_tree(
            &output,
            &composed,
            &non_current_owner(),
            &CurrentOwnerOnlyFsTreeOwnerApplier,
        )
        .unwrap_err();

        assert!(matches!(error, FsTreeComposeError::Invalid(_)));
        assert!(!output.exists());
    }
}
