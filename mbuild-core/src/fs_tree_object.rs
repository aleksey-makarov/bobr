use crate::{FsTreeEntry, FsTreeManifest, FsTreeManifestError};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MANIFEST_FILE_NAME: &str = "manifest.jsonl";
const ROOT_DIR_NAME: &str = "root";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeObjectError {
    Invalid(String),
    Io(String),
    Manifest(FsTreeManifestError),
}

impl fmt::Display for FsTreeObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Io(message) => f.write_str(message),
            Self::Manifest(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FsTreeObjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Invalid(_) | Self::Io(_) => None,
        }
    }
}

impl From<FsTreeManifestError> for FsTreeObjectError {
    fn from(error: FsTreeManifestError) -> Self {
        Self::Manifest(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeObjectPaths {
    pub object_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub root_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedFsTreeObject {
    pub paths: FsTreeObjectPaths,
    pub manifest: FsTreeManifest,
}

pub trait FsTreeOwnerMap {
    fn physical_uid(&self, logical_uid: u32) -> Result<u32, FsTreeObjectError>;
    fn physical_gid(&self, logical_gid: u32) -> Result<u32, FsTreeObjectError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityFsTreeOwnerMap;

impl FsTreeOwnerMap for IdentityFsTreeOwnerMap {
    fn physical_uid(&self, logical_uid: u32) -> Result<u32, FsTreeObjectError> {
        Ok(logical_uid)
    }

    fn physical_gid(&self, logical_gid: u32) -> Result<u32, FsTreeObjectError> {
        Ok(logical_gid)
    }
}

pub fn validate_fs_tree_object(
    object_dir: &Path,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<ValidatedFsTreeObject, FsTreeObjectError> {
    #[cfg(not(unix))]
    {
        let _ = object_dir;
        let _ = owner_map;
        return Err(FsTreeObjectError::Invalid(
            "fs-tree object validation is only supported on unix hosts".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        let paths = FsTreeObjectPaths {
            object_dir: object_dir.to_path_buf(),
            manifest_path: object_dir.join(MANIFEST_FILE_NAME),
            root_dir: object_dir.join(ROOT_DIR_NAME),
        };

        require_directory(object_dir, "fs-tree object directory")?;
        validate_top_level_shape(&paths)?;

        let manifest = FsTreeManifest::read_canonical(&paths.manifest_path)?;
        validate_root_against_manifest(&paths.root_dir, &manifest, owner_map)?;

        Ok(ValidatedFsTreeObject { paths, manifest })
    }
}

pub fn create_fs_tree_staging_dir(
    object_dir: &Path,
    manifest: &FsTreeManifest,
) -> Result<FsTreeObjectPaths, FsTreeObjectError> {
    if object_dir.exists() || object_dir.is_symlink() {
        return Err(FsTreeObjectError::Invalid(format!(
            "fs-tree staging directory already exists: '{}'",
            object_dir.display()
        )));
    }

    fs::create_dir(object_dir).map_err(|error| {
        FsTreeObjectError::Io(format!(
            "failed to create fs-tree staging directory '{}': {error}",
            object_dir.display()
        ))
    })?;

    let paths = FsTreeObjectPaths {
        object_dir: object_dir.to_path_buf(),
        manifest_path: object_dir.join(MANIFEST_FILE_NAME),
        root_dir: object_dir.join(ROOT_DIR_NAME),
    };

    manifest.write_canonical(&paths.manifest_path)?;
    fs::create_dir(&paths.root_dir).map_err(|error| {
        FsTreeObjectError::Io(format!(
            "failed to create fs-tree root directory '{}': {error}",
            paths.root_dir.display()
        ))
    })?;
    set_root_mode_from_manifest(&paths.root_dir, manifest)?;

    Ok(paths)
}

#[cfg(unix)]
fn validate_top_level_shape(paths: &FsTreeObjectPaths) -> Result<(), FsTreeObjectError> {
    let mut entries = HashSet::new();
    for entry in fs::read_dir(&paths.object_dir).map_err(|error| {
        FsTreeObjectError::Io(format!(
            "failed to read fs-tree object directory '{}': {error}",
            paths.object_dir.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            FsTreeObjectError::Io(format!(
                "failed to read fs-tree object entry in '{}': {error}",
                paths.object_dir.display()
            ))
        })?;
        let name = entry.file_name();
        if name == OsStr::new(MANIFEST_FILE_NAME) {
            entries.insert(MANIFEST_FILE_NAME);
        } else if name == OsStr::new(ROOT_DIR_NAME) {
            entries.insert(ROOT_DIR_NAME);
        }
    }

    if !entries.contains(MANIFEST_FILE_NAME) {
        return Err(FsTreeObjectError::Invalid(format!(
            "fs-tree object '{}' is missing manifest.jsonl",
            paths.object_dir.display()
        )));
    }
    if !entries.contains(ROOT_DIR_NAME) {
        return Err(FsTreeObjectError::Invalid(format!(
            "fs-tree object '{}' is missing root directory",
            paths.object_dir.display()
        )));
    }

    require_regular_non_executable_file(&paths.manifest_path, "fs-tree manifest")?;
    require_directory(&paths.root_dir, "fs-tree root directory")
}

#[cfg(unix)]
fn validate_root_against_manifest(
    root_dir: &Path,
    manifest: &FsTreeManifest,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), FsTreeObjectError> {
    let mut manifest_entries = HashMap::with_capacity(manifest.entries().len());
    for entry in manifest.entries() {
        manifest_entries.insert(entry.path(), entry);
    }

    let mut seen = HashSet::with_capacity(manifest.entries().len());
    let root_entry = manifest_entries.get("").ok_or_else(|| {
        FsTreeObjectError::Invalid("fs-tree manifest is missing root entry".to_string())
    })?;
    validate_root_entry(root_dir, root_entry, owner_map)?;
    seen.insert(String::new());

    scan_root_dir(root_dir, "", &manifest_entries, &mut seen, owner_map)?;

    for entry in manifest.entries() {
        if !seen.contains(entry.path()) {
            return Err(FsTreeObjectError::Invalid(format!(
                "fs-tree manifest entry '{}' is missing from root",
                entry.path()
            )));
        }
    }

    Ok(())
}

#[cfg(unix)]
fn scan_root_dir(
    dir: &Path,
    rel_dir: &str,
    manifest_entries: &HashMap<&str, &FsTreeEntry>,
    seen: &mut HashSet<String>,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), FsTreeObjectError> {
    for entry in fs::read_dir(dir).map_err(|error| {
        FsTreeObjectError::Io(format!(
            "failed to read fs-tree root '{}': {error}",
            dir.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            FsTreeObjectError::Io(format!(
                "failed to read fs-tree root entry in '{}': {error}",
                dir.display()
            ))
        })?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            FsTreeObjectError::Invalid(format!(
                "fs-tree root contains non-UTF-8 path under '{}'",
                dir.display()
            ))
        })?;
        let rel_path = if rel_dir.is_empty() {
            name.to_string()
        } else {
            format!("{rel_dir}/{name}")
        };

        let manifest_entry = manifest_entries.get(rel_path.as_str()).ok_or_else(|| {
            FsTreeObjectError::Invalid(format!(
                "fs-tree root entry '{}' is missing from manifest",
                rel_path
            ))
        })?;

        validate_non_root_entry(&entry.path(), &rel_path, manifest_entry, owner_map)?;
        seen.insert(rel_path.clone());

        if entry
            .file_type()
            .map_err(|error| {
                FsTreeObjectError::Io(format!(
                    "failed to inspect fs-tree root entry '{}': {error}",
                    entry.path().display()
                ))
            })?
            .is_dir()
        {
            scan_root_dir(&entry.path(), &rel_path, manifest_entries, seen, owner_map)?;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn validate_root_entry(
    path: &Path,
    manifest_entry: &FsTreeEntry,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), FsTreeObjectError> {
    match manifest_entry {
        FsTreeEntry::Directory { .. } => {
            validate_path_against_entry(path, "", manifest_entry, owner_map)
        }
        _ => Err(FsTreeObjectError::Invalid(
            "fs-tree manifest root entry must be a directory".to_string(),
        )),
    }
}

#[cfg(unix)]
fn validate_non_root_entry(
    path: &Path,
    rel_path: &str,
    manifest_entry: &FsTreeEntry,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), FsTreeObjectError> {
    validate_path_against_entry(path, rel_path, manifest_entry, owner_map)
}

#[cfg(unix)]
fn validate_path_against_entry(
    path: &Path,
    rel_path: &str,
    manifest_entry: &FsTreeEntry,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), FsTreeObjectError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        FsTreeObjectError::Io(format!(
            "failed to inspect fs-tree entry '{}': {error}",
            path.display()
        ))
    })?;
    let file_type = metadata.file_type();

    match manifest_entry {
        FsTreeEntry::File { uid, gid, mode, .. } => {
            if !file_type.is_file() {
                return Err(kind_mismatch(rel_path, "file", &file_type));
            }
            validate_owner(rel_path, &metadata, *uid, *gid, owner_map)?;
            validate_mode(rel_path, &metadata, *mode)?;
        }
        FsTreeEntry::Directory { uid, gid, mode, .. } => {
            if !file_type.is_dir() {
                return Err(kind_mismatch(rel_path, "directory", &file_type));
            }
            validate_owner(rel_path, &metadata, *uid, *gid, owner_map)?;
            validate_mode(rel_path, &metadata, *mode)?;
        }
        FsTreeEntry::Symlink { uid, gid, .. } => {
            if !file_type.is_symlink() {
                return Err(kind_mismatch(rel_path, "symlink", &file_type));
            }
            validate_owner(rel_path, &metadata, *uid, *gid, owner_map)?;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn validate_owner(
    rel_path: &str,
    metadata: &fs::Metadata,
    logical_uid: u32,
    logical_gid: u32,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<(), FsTreeObjectError> {
    let expected_uid = owner_map.physical_uid(logical_uid)?;
    let expected_gid = owner_map.physical_gid(logical_gid)?;
    let actual_uid = metadata.uid();
    let actual_gid = metadata.gid();

    if actual_uid != expected_uid {
        return Err(FsTreeObjectError::Invalid(format!(
            "uid mismatch for fs-tree entry '{}': expected physical uid {}, got {}",
            rel_path, expected_uid, actual_uid
        )));
    }
    if actual_gid != expected_gid {
        return Err(FsTreeObjectError::Invalid(format!(
            "gid mismatch for fs-tree entry '{}': expected physical gid {}, got {}",
            rel_path, expected_gid, actual_gid
        )));
    }

    Ok(())
}

#[cfg(unix)]
fn validate_mode(
    rel_path: &str,
    metadata: &fs::Metadata,
    expected_mode: u32,
) -> Result<(), FsTreeObjectError> {
    let actual_mode = metadata.permissions().mode() & 0o7777;
    if actual_mode != expected_mode {
        return Err(FsTreeObjectError::Invalid(format!(
            "mode mismatch for fs-tree entry '{}': expected {:o}, got {:o}",
            rel_path, expected_mode, actual_mode
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn kind_mismatch(rel_path: &str, expected: &str, file_type: &fs::FileType) -> FsTreeObjectError {
    FsTreeObjectError::Invalid(format!(
        "kind mismatch for fs-tree entry '{}': expected {}, got {}",
        rel_path,
        expected,
        actual_kind(file_type)
    ))
}

#[cfg(unix)]
fn actual_kind(file_type: &fs::FileType) -> &'static str {
    if file_type.is_file() {
        "file"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symlink"
    } else {
        "unsupported"
    }
}

#[cfg(unix)]
fn require_regular_non_executable_file(path: &Path, label: &str) -> Result<(), FsTreeObjectError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        FsTreeObjectError::Io(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(FsTreeObjectError::Invalid(format!(
            "{label} '{}' must be a regular file",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o111 != 0 {
        return Err(FsTreeObjectError::Invalid(format!(
            "{label} '{}' must not be executable",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn require_directory(path: &Path, label: &str) -> Result<(), FsTreeObjectError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        FsTreeObjectError::Io(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_dir() {
        return Err(FsTreeObjectError::Invalid(format!(
            "{label} '{}' must be a directory",
            path.display()
        )));
    }
    Ok(())
}

fn set_root_mode_from_manifest(
    root_dir: &Path,
    manifest: &FsTreeManifest,
) -> Result<(), FsTreeObjectError> {
    #[cfg(unix)]
    {
        let mode = manifest
            .entries()
            .first()
            .and_then(|entry| match entry {
                FsTreeEntry::Directory { path, mode, .. } if path.is_empty() => Some(*mode),
                _ => None,
            })
            .ok_or_else(|| {
                FsTreeObjectError::Invalid(
                    "fs-tree manifest must contain a root directory entry".to_string(),
                )
            })?;
        fs::set_permissions(root_dir, fs::Permissions::from_mode(mode)).map_err(|error| {
            FsTreeObjectError::Io(format!(
                "failed to set fs-tree root directory mode '{}': {error}",
                root_dir.display()
            ))
        })
    }

    #[cfg(not(unix))]
    {
        let _ = root_dir;
        let _ = manifest;
        Ok(())
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
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

    fn current_owner() -> ConstantOwnerMap {
        let metadata = fs::metadata(".").unwrap();
        ConstantOwnerMap {
            uid: metadata.uid(),
            gid: metadata.gid(),
        }
    }

    fn mismatched_owner() -> ConstantOwnerMap {
        let owner = current_owner();
        ConstantOwnerMap {
            uid: owner.uid.saturating_add(1),
            gid: owner.gid,
        }
    }

    fn root(uid: u32, gid: u32) -> FsTreeEntry {
        FsTreeEntry::directory("", uid, gid, 0o755)
    }

    fn make_manifest(entries: Vec<FsTreeEntry>) -> FsTreeManifest {
        FsTreeManifest::from_entries(entries).unwrap()
    }

    fn write_file(path: &Path, mode: u32) {
        let mut file = File::create(path).unwrap();
        file.write_all(b"payload\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn create_valid_object(base: &Path) -> (PathBuf, FsTreeManifest, ConstantOwnerMap) {
        let owner = current_owner();
        let object_dir = base.join("object");
        let manifest = make_manifest(vec![
            root(0, 0),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file("bin/tool", 0, 0, 0o644),
            FsTreeEntry::symlink("bin/tool-link", 0, 0),
        ]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        fs::create_dir(paths.root_dir.join("bin")).unwrap();
        fs::set_permissions(
            paths.root_dir.join("bin"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write_file(&paths.root_dir.join("bin/tool"), 0o644);
        symlink("tool", paths.root_dir.join("bin/tool-link")).unwrap();
        (object_dir, manifest, owner)
    }

    #[test]
    fn accepts_valid_object_with_files_directories_and_symlinks() {
        let temp = tempdir().unwrap();
        let (object_dir, manifest, owner) = create_valid_object(temp.path());

        let validated = validate_fs_tree_object(&object_dir, &owner).unwrap();

        assert_eq!(validated.paths.object_dir, object_dir);
        assert_eq!(validated.manifest, manifest);
    }

    #[test]
    fn staging_helper_writes_manifest_creates_root_and_rejects_existing_target() {
        let temp = tempdir().unwrap();
        let owner = current_owner();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(owner.uid, owner.gid)]);

        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();

        assert_eq!(paths.object_dir, object_dir);
        assert_eq!(
            FsTreeManifest::read_canonical(&paths.manifest_path).unwrap(),
            manifest
        );
        assert!(paths.root_dir.is_dir());
        assert!(create_fs_tree_staging_dir(&paths.object_dir, &manifest).is_err());
    }

    #[test]
    fn rejects_bad_top_level_shape() {
        let temp = tempdir().unwrap();
        let owner = current_owner();

        assert!(validate_fs_tree_object(&temp.path().join("missing"), &owner).is_err());

        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(owner.uid, owner.gid)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();

        fs::write(object_dir.join("extra"), b"extra").unwrap();
        assert!(validate_fs_tree_object(&object_dir, &owner).is_ok());
        fs::remove_file(object_dir.join("extra")).unwrap();

        fs::remove_file(&paths.manifest_path).unwrap();
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());
        manifest.write_canonical(&paths.manifest_path).unwrap();

        fs::remove_dir(&paths.root_dir).unwrap();
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());
    }

    #[test]
    fn rejects_manifest_or_root_with_wrong_top_level_type_or_mode() {
        let temp = tempdir().unwrap();
        let owner = current_owner();

        let manifest_dir = temp.path().join("manifest-dir");
        fs::create_dir(&manifest_dir).unwrap();
        fs::create_dir(manifest_dir.join(MANIFEST_FILE_NAME)).unwrap();
        fs::create_dir(manifest_dir.join(ROOT_DIR_NAME)).unwrap();
        assert!(validate_fs_tree_object(&manifest_dir, &owner).is_err());

        let root_file = temp.path().join("root-file");
        fs::create_dir(&root_file).unwrap();
        let manifest = make_manifest(vec![root(owner.uid, owner.gid)]);
        manifest
            .write_canonical(&root_file.join(MANIFEST_FILE_NAME))
            .unwrap();
        fs::write(root_file.join(ROOT_DIR_NAME), b"not a directory").unwrap();
        assert!(validate_fs_tree_object(&root_file, &owner).is_err());

        let exec_manifest = temp.path().join("exec-manifest");
        let paths = create_fs_tree_staging_dir(&exec_manifest, &manifest).unwrap();
        fs::set_permissions(&paths.manifest_path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_fs_tree_object(&exec_manifest, &owner).is_err());

        let manifest_symlink = temp.path().join("manifest-symlink");
        fs::create_dir(&manifest_symlink).unwrap();
        fs::write(manifest_symlink.join("target"), b"not a manifest").unwrap();
        symlink("target", manifest_symlink.join(MANIFEST_FILE_NAME)).unwrap();
        fs::create_dir(manifest_symlink.join(ROOT_DIR_NAME)).unwrap();
        assert!(validate_fs_tree_object(&manifest_symlink, &owner).is_err());

        let root_symlink = temp.path().join("root-symlink");
        fs::create_dir(&root_symlink).unwrap();
        manifest
            .write_canonical(&root_symlink.join(MANIFEST_FILE_NAME))
            .unwrap();
        fs::create_dir(root_symlink.join("target-root")).unwrap();
        symlink("target-root", root_symlink.join(ROOT_DIR_NAME)).unwrap();
        assert!(validate_fs_tree_object(&root_symlink, &owner).is_err());
    }

    #[test]
    fn rejects_non_canonical_manifest() {
        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        fs::create_dir(&object_dir).unwrap();
        fs::write(
            object_dir.join(MANIFEST_FILE_NAME),
            br#"{"t":"d","p":"","u":0,"g":0,"m":493}
"#,
        )
        .unwrap();
        fs::create_dir(object_dir.join(ROOT_DIR_NAME)).unwrap();

        assert!(validate_fs_tree_object(&object_dir, &current_owner()).is_err());
    }

    #[test]
    fn rejects_manifest_entry_missing_from_root_and_extra_root_entry() {
        let temp = tempdir().unwrap();
        let owner = current_owner();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0), FsTreeEntry::file("missing", 0, 0, 0o644)]);
        create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();

        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());

        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        write_file(&paths.root_dir.join("extra"), 0o644);

        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());
    }

    #[test]
    fn rejects_kind_mismatches() {
        let owner = current_owner();

        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0), FsTreeEntry::file("entry", 0, 0, 0o644)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        fs::create_dir(paths.root_dir.join("entry")).unwrap();
        fs::set_permissions(
            paths.root_dir.join("entry"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());

        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![
            root(0, 0),
            FsTreeEntry::directory("entry", 0, 0, 0o755),
        ]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        write_file(&paths.root_dir.join("entry"), 0o755);
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());

        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0), FsTreeEntry::symlink("entry", 0, 0)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        write_file(&paths.root_dir.join("entry"), 0o644);
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());
    }

    #[test]
    fn rejects_file_and_directory_mode_mismatches() {
        let owner = current_owner();

        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        fs::set_permissions(&paths.root_dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());

        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0), FsTreeEntry::file("file", 0, 0, 0o755)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        write_file(&paths.root_dir.join("file"), 0o644);
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());

        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0), FsTreeEntry::directory("dir", 0, 0, 0o700)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        fs::create_dir(paths.root_dir.join("dir")).unwrap();
        fs::set_permissions(
            paths.root_dir.join("dir"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());
    }

    #[test]
    fn accepts_symlink_without_mode_comparison() {
        let temp = tempdir().unwrap();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0), FsTreeEntry::symlink("link", 0, 0)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        symlink("target", paths.root_dir.join("link")).unwrap();

        assert!(validate_fs_tree_object(&object_dir, &current_owner()).is_ok());
    }

    #[test]
    fn rejects_owner_mismatch_and_accepts_mapped_owner() {
        let temp = tempdir().unwrap();
        let (object_dir, _, owner) = create_valid_object(temp.path());

        assert!(validate_fs_tree_object(&object_dir, &mismatched_owner()).is_err());
        assert!(validate_fs_tree_object(&object_dir, &owner).is_ok());
    }

    #[test]
    fn rejects_non_utf8_filesystem_paths_under_root() {
        let temp = tempdir().unwrap();
        let owner = current_owner();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        let name = std::ffi::OsString::from_vec(vec![0xff]);
        File::create(paths.root_dir.join(name)).unwrap();

        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());
    }

    #[test]
    fn rejects_unsupported_filesystem_entry_kinds() {
        let temp = tempdir().unwrap();
        let owner = current_owner();
        let object_dir = temp.path().join("object");
        let manifest = make_manifest(vec![root(0, 0), FsTreeEntry::file("socket", 0, 0, 0o644)]);
        let paths = create_fs_tree_staging_dir(&object_dir, &manifest).unwrap();
        let _listener = UnixListener::bind(paths.root_dir.join("socket")).unwrap();

        assert!(validate_fs_tree_object(&object_dir, &owner).is_err());
    }
}
