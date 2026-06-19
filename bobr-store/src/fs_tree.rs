//! Canonical `fs-tree manifest` parser, writer, validation types, and
//! local root-runner filesystem operations.
//!
//! This crate owns the manifest-addressed fs-tree manifest format. It
//! does not implement builder integration or the legacy fs-tree object format.

#![deny(missing_docs)]

use crate::StoreError;
use crate::store::{FS_FILES_DIR, FS_TREES_DIR, OBJECTS_DIR};
use globset::{Glob, GlobMatcher};
use mbuild_core::ObjectHash;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const SCHEMA: &str = "bobr-fs-tree-manifest";
const CANONICAL_SCHEMA_LINE: &[u8] = br#"{"schema":"bobr-fs-tree-manifest"}
"#;
const FS_FILE_HASH_TAG: &[u8] = b"bobr:fs-file:v1\0";

/// Store-scoped access to fs-tree operations.
///
/// This value is intentionally opaque: callers can obtain it from
/// [`crate::Store::fs_tree`] or through serde deserialization, but cannot
/// construct it directly. It carries only enough store context for fs-tree
/// operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTree {
    root: PathBuf,
}

/// Canonical `fs-tree manifest`.
///
/// A manifest contains validated filesystem entries sorted by UTF-8 path bytes.
/// The serialized form always starts with the
/// `{"schema":"bobr-fs-tree-manifest"}` header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeManifest {
    entries: Vec<FsTreeEntry>,
}

/// One filesystem entry in an `fs-tree manifest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeEntry {
    /// Regular file entry.
    ///
    /// The entry stores only the path and an opaque future fs-file object hash.
    /// Regular file bytes, owner, group, and mode are intentionally not stored
    /// in manifest.
    File {
        /// Relative UTF-8 path inside the fs-tree.
        path: String,
        /// Opaque future fs-file object hash.
        hash: FsFileHash,
    },
    /// Directory entry with logical ownership and mode metadata.
    Directory {
        /// Relative UTF-8 path inside the fs-tree. The root directory is `""`.
        path: String,
        /// Logical uid.
        uid: u32,
        /// Logical gid.
        gid: u32,
        /// Directory mode, constrained to `0..=0o7777`.
        mode: u32,
    },
    /// Symlink entry with logical ownership metadata and a literal target.
    Symlink {
        /// Relative UTF-8 path inside the fs-tree.
        path: String,
        /// Logical uid.
        uid: u32,
        /// Logical gid.
        gid: u32,
        /// UTF-8 symlink target.
        target: String,
    },
}

/// Filesystem entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsTreeEntryKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symlink.
    Symlink,
}

/// Opaque future fs-file object hash used by manifest regular file entries.
///
/// The type validates and formats exactly 64 lowercase hex digits. The hash
/// algorithm is deliberately not defined in this crate yet.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FsFileHash([u8; 32]);

/// Install policy used when importing an existing filesystem tree into
/// manifest.
///
/// Rules are evaluated in order. Every rule whose glob pattern matches a path
/// contributes its explicitly specified attributes; later matching rules
/// override earlier values field-by-field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsTreeInstall {
    /// Ordered install rules.
    pub rules: Vec<FsTreeInstallRule>,
}

/// One glob-based install rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsTreeInstallRule {
    /// Glob pattern matched against manifest-relative UTF-8 paths.
    ///
    /// The root directory path is the empty string. A pattern ending in `/**`
    /// also matches the prefix path itself.
    pub path: String,
    /// Attributes contributed by this rule.
    pub attrs: FsTreeInstallAttrs,
}

/// Filesystem attributes contributed by an install rule.
///
/// Attribute values are optional so that more specific rules can override only
/// selected fields while inheriting the rest from broader rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsTreeInstallAttrs {
    /// Logical uid for directories, files, and symlinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Logical gid for directories, files, and symlinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// Directory mode, constrained to `0..=0o7777`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_mode: Option<u32>,
    /// Regular non-executable file mode, constrained to `0..=0o7777`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regular_file_mode: Option<u32>,
    /// Executable regular file mode, constrained to `0..=0o7777`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_file_mode: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsTreeWire {
    root: PathBuf,
}

/// Error returned when parsing an [`FsFileHash`] from text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseFsFileHashError {
    /// The string is not exactly 64 bytes long.
    InvalidLength,
    /// The string contains a byte outside `[0-9a-f]`.
    InvalidHex,
}

impl fmt::Display for ParseFsFileHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

impl std::error::Error for ParseFsFileHashError {}

impl FsTree {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn fs_files_dir(&self) -> PathBuf {
        self.root.join(FS_FILES_DIR)
    }

    fn fs_trees_dir(&self) -> PathBuf {
        self.root.join(FS_TREES_DIR)
    }

    fn object_path(&self, object_hash: ObjectHash) -> PathBuf {
        self.root.join(OBJECTS_DIR).join(object_hash.to_hex())
    }

    fn materialized_root_path(&self, manifest_hash: ObjectHash) -> PathBuf {
        self.fs_trees_dir().join(manifest_hash.to_hex())
    }

    /// Scans an immutable filesystem tree into a manifest and imports regular
    /// files into this store's `fs-files` directory using hardlinks.
    ///
    /// The source root must be an absolute existing real directory, not a
    /// symlink. Regular files are hardlinked into `fs-files`; if the target
    /// object already exists, it is trusted as a cache hit. Shard directories
    /// are created lazily for objects that need to be imported.
    pub fn scan(&self, source_root: &Path) -> Result<FsTreeManifest, StoreError> {
        scan_fs_tree_with_root(source_root, &self.fs_files_dir())
    }

    /// Imports a filesystem tree into a manifest and this store's `fs-files`
    /// directory using install rules.
    ///
    /// The source root must be an absolute existing real directory, not a
    /// symlink. Regular files are copied into `fs-files` with the installed
    /// uid, gid, and mode encoded into the fs-file hash. The source tree is
    /// never modified.
    pub fn import_with_install(
        &self,
        source_root: &Path,
        install: &FsTreeInstall,
    ) -> Result<FsTreeManifest, StoreError> {
        import_fs_tree_with_install_root(source_root, &self.fs_files_dir(), install)
    }

    /// Returns the materialized cache root for `manifest_hash` if it already
    /// exists.
    ///
    /// The cache root is `<store>/fs-trees/<manifest_hash>`. This method does
    /// not read or validate the manifest object; it only checks whether the
    /// cache path already contains a real directory. Any other existing file
    /// type at the cache path is treated as corrupt store data.
    pub fn lookup_materialized_root(
        &self,
        manifest_hash: ObjectHash,
    ) -> Result<Option<PathBuf>, StoreError> {
        validate_fs_tree_root(&self.root)?;
        let root = self.materialized_root_path(manifest_hash);
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(Some(root)),
            Ok(_) => Err(StoreError::InvalidData(format!(
                "fs-tree materialization cache path exists but is not a real directory: '{}'",
                root.display()
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_io(
                &root,
                "inspect fs-tree materialization cache",
                error,
            )),
        }
    }

    /// Ensures that `<store>/fs-trees/<manifest_hash>` contains the
    /// materialized filesystem tree for the canonical manifest object.
    ///
    /// On cache hit this returns the existing cache root without reading the
    /// manifest. On miss it reads `<store>/objects/<manifest_hash>`, parses it
    /// as a canonical [`FsTreeManifest`], materializes it from `fs-files` into
    /// a store-owned staging directory, and publishes that directory as the
    /// cache root.
    pub fn ensure_materialized_root(
        &self,
        manifest_hash: ObjectHash,
    ) -> Result<PathBuf, StoreError> {
        if let Some(root) = self.lookup_materialized_root(manifest_hash)? {
            return Ok(root);
        }

        let manifest = FsTreeManifest::read_canonical(&self.object_path(manifest_hash))?;
        let final_root = self.materialized_root_path(manifest_hash);
        let staging_root = create_unique_fs_tree_staging_dir(&self.fs_trees_dir(), manifest_hash)?;
        let result = materialize_fs_tree_with_root(&manifest, &self.fs_files_dir(), &staging_root)
            .and_then(|()| publish_materialized_fs_tree(&staging_root, &final_root));
        match result {
            Ok(()) => Ok(final_root),
            Err(error) => {
                let _ = fs::remove_dir_all(&staging_root);
                if let Some(root) = self.lookup_materialized_root(manifest_hash)? {
                    Ok(root)
                } else {
                    Err(error)
                }
            }
        }
    }
}

impl Serialize for FsTree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        FsTreeWire {
            root: self.root.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FsTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FsTreeWire::deserialize(deserializer)?;
        validate_fs_tree_root(&wire.root).map_err(de::Error::custom)?;
        Ok(Self::new(wire.root))
    }
}

impl FsTreeManifest {
    /// Builds a manifest from entries.
    ///
    /// Entries are sorted by path bytes before validation. The resulting
    /// manifest must contain a root directory entry, must not contain duplicate
    /// paths, and every non-root entry must have an explicit directory parent.
    pub fn from_entries(mut entries: Vec<FsTreeEntry>) -> Result<Self, StoreError> {
        entries.sort_by(|left, right| left.path().as_bytes().cmp(right.path().as_bytes()));
        validate_entries(&entries)?;
        Ok(Self { entries })
    }

    /// Parses canonical manifest bytes.
    ///
    /// This accepts only the exact canonical encoding produced by
    /// [`FsTreeManifest::to_canonical_bytes`]. Non-canonical JSON whitespace,
    /// alternate field order, missing final newline, unknown fields, duplicate
    /// paths, malformed paths, and missing parent directories are rejected.
    pub fn parse_canonical_bytes(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.is_empty() {
            return Err(StoreError::InvalidData(
                "fs-tree manifest must not be empty".to_string(),
            ));
        }
        if !bytes.ends_with(b"\n") {
            return Err(StoreError::InvalidData(
                "fs-tree manifest must end with a newline".to_string(),
            ));
        }

        let text = std::str::from_utf8(bytes).map_err(|error| {
            StoreError::InvalidData(format!("fs-tree manifest is not UTF-8: {error}"))
        })?;

        let mut lines = text.split_inclusive('\n');
        let Some(schema_line) = lines.next() else {
            return Err(StoreError::InvalidData(
                "fs-tree manifest is missing schema header".to_string(),
            ));
        };
        validate_schema_header(schema_line)?;

        let mut entries = Vec::new();
        for (index, raw_line) in lines.enumerate() {
            let line_number = index + 2;
            let line = raw_line.strip_suffix('\n').expect("split line has suffix");
            if line.is_empty() {
                return Err(StoreError::InvalidData(format!(
                    "fs-tree manifest line {line_number} is empty"
                )));
            }

            let entry = parse_entry_line(line, line_number)?;
            let canonical = canonical_entry_bytes(&entry)?;
            if canonical.as_slice() != raw_line.as_bytes() {
                return Err(StoreError::InvalidData(format!(
                    "fs-tree manifest line {line_number} is not canonical"
                )));
            }
            entries.push(entry);
        }

        let manifest = Self::from_entries(entries)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let canonical = manifest.to_canonical_bytes()?;
        if canonical.as_slice() != bytes {
            return Err(StoreError::InvalidData(
                "fs-tree manifest entries are not in canonical order".to_string(),
            ));
        }
        Ok(manifest)
    }

    /// Reads and parses a canonical manifest file.
    pub fn read_canonical(path: &Path) -> Result<Self, StoreError> {
        let bytes = fs::read(path).map_err(|error| {
            StoreError::Io(format!(
                "failed to read fs-tree manifest '{}': {error}",
                path.display()
            ))
        })?;
        Self::parse_canonical_bytes(&bytes)
    }

    /// Serializes this manifest to canonical JSONL bytes.
    ///
    /// The output always includes the schema header and a final newline.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        validate_entries(&self.entries)?;
        let mut out = Vec::new();
        out.extend_from_slice(CANONICAL_SCHEMA_LINE);
        for entry in &self.entries {
            write_canonical_entry(entry, &mut out)?;
        }
        Ok(out)
    }

    /// Writes this manifest as canonical JSONL bytes.
    pub fn write_canonical(&self, path: &Path) -> Result<(), StoreError> {
        let bytes = self.to_canonical_bytes()?;
        fs::write(path, bytes).map_err(|error| {
            StoreError::Io(format!(
                "failed to write fs-tree manifest '{}': {error}",
                path.display()
            ))
        })
    }

    /// Returns validated manifest entries in canonical path order.
    pub fn entries(&self) -> &[FsTreeEntry] {
        &self.entries
    }
}

/// Selects a subset of a canonical fs-tree manifest.
///
/// `include` contains relative glob patterns matched against manifest paths.
/// A pattern ending in `/**` also matches the prefix path itself. The root
/// entry is not selected as a payload entry, but required parent directories
/// are added automatically for every selected non-root path.
///
/// This is a pure manifest operation. It does not inspect `fs-files`, does not
/// materialize an fs-tree root, and does not verify that regular file payloads
/// exist in any store.
pub fn subset_manifest(
    manifest: &FsTreeManifest,
    include: &[String],
) -> Result<FsTreeManifest, StoreError> {
    let patterns = compile_fs_tree_subset_patterns(include)?;
    let by_path = manifest
        .entries()
        .iter()
        .map(|entry| (entry.path(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeSet::<String>::new();

    for entry in manifest.entries() {
        let path = entry.path();
        if path.is_empty() {
            continue;
        }
        if fs_tree_subset_path_matches(path, &patterns) {
            selected.insert(path.to_string());
            add_fs_tree_subset_parent_dirs(path, &by_path, &mut selected)?;
        }
    }

    if !selected.iter().any(|path| !path.is_empty()) {
        return Err(StoreError::InvalidInput(
            "fs-tree subset include patterns selected no paths".to_string(),
        ));
    }

    let entries = manifest
        .entries()
        .iter()
        .filter(|entry| selected.contains(entry.path()))
        .cloned()
        .collect();
    FsTreeManifest::from_entries(entries)
}

/// Merges canonical fs-tree manifests.
///
/// Entries are merged by path. Identical overlaps are allowed, including
/// matching directory entries with the same uid, gid, and mode. Any kind,
/// metadata, symlink target, or regular file hash conflict is rejected.
///
/// This is a pure manifest operation. It does not inspect `fs-files`, does not
/// materialize an fs-tree root, and does not verify that regular file payloads
/// exist in any store.
pub fn merge_manifests(manifests: &[FsTreeManifest]) -> Result<FsTreeManifest, StoreError> {
    if manifests.is_empty() {
        return Err(StoreError::InvalidInput(
            "fs-tree merge requires at least one manifest".to_string(),
        ));
    }

    let mut by_path = BTreeMap::<String, FsTreeEntry>::new();
    for manifest in manifests {
        for entry in manifest.entries() {
            match by_path.get(entry.path()) {
                Some(existing) if existing == entry => {}
                Some(existing) => {
                    return Err(StoreError::InvalidInput(format!(
                        "conflicting fs-tree entries at '{}': {}",
                        entry.path(),
                        fs_tree_merge_conflict_reason(existing, entry)
                    )));
                }
                None => {
                    by_path.insert(entry.path().to_string(), entry.clone());
                }
            }
        }
    }

    FsTreeManifest::from_entries(by_path.into_values().collect())
}

impl FsTreeEntry {
    /// Creates a regular file entry.
    pub fn file(path: impl Into<String>, hash: FsFileHash) -> Self {
        Self::File {
            path: path.into(),
            hash,
        }
    }

    /// Creates a directory entry.
    pub fn directory(path: impl Into<String>, uid: u32, gid: u32, mode: u32) -> Self {
        Self::Directory {
            path: path.into(),
            uid,
            gid,
            mode,
        }
    }

    /// Creates a symlink entry.
    pub fn symlink(path: impl Into<String>, uid: u32, gid: u32, target: impl Into<String>) -> Self {
        Self::Symlink {
            path: path.into(),
            uid,
            gid,
            target: target.into(),
        }
    }

    /// Returns the entry path.
    pub fn path(&self) -> &str {
        match self {
            Self::File { path, .. } | Self::Directory { path, .. } | Self::Symlink { path, .. } => {
                path
            }
        }
    }

    /// Returns the entry kind.
    pub fn kind(&self) -> FsTreeEntryKind {
        match self {
            Self::File { .. } => FsTreeEntryKind::File,
            Self::Directory { .. } => FsTreeEntryKind::Directory,
            Self::Symlink { .. } => FsTreeEntryKind::Symlink,
        }
    }
}

impl FsFileHash {
    /// Returns the raw 32-byte hash value.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Formats this hash as 64 lowercase hex digits.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }
}

impl fmt::Display for FsFileHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for FsFileHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FsFileHash").field(&self.to_hex()).finish()
    }
}

impl FromStr for FsFileHash {
    type Err = ParseFsFileHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(ParseFsFileHashError::InvalidLength);
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ParseFsFileHashError::InvalidHex);
        }

        let mut bytes = [0u8; 32];
        for (idx, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let hi = decode_nibble(chunk[0]).ok_or(ParseFsFileHashError::InvalidHex)?;
            let lo = decode_nibble(chunk[1]).ok_or(ParseFsFileHashError::InvalidHex)?;
            bytes[idx] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

/// Returns the path of an fs-file object under an `fs-files` root.
///
/// The root path must be absolute. The layout is
/// `<root>/<first-two-hex-digits>/<64-lowercase-hex-hash>`.
fn fs_file_path(root: &Path, hash: FsFileHash) -> Result<PathBuf, StoreError> {
    require_absolute(root, "fs-files root")?;
    let hex = hash.to_hex();
    Ok(root.join(&hex[..2]).join(hex))
}

/// Hashes a regular file at an absolute filesystem path.
///
/// The file is hashed using the fs-file hash algorithm with the file's current
/// uid, gid, mode, size, and byte content. Symlinks and non-regular files are
/// rejected.
fn hash_fs_file_path(path: &Path) -> Result<FsFileHash, StoreError> {
    require_absolute(path, "fs-file path")?;

    let metadata =
        fs::symlink_metadata(path).map_err(|error| map_io(path, "inspect fs-file", error))?;
    if !metadata.file_type().is_file() {
        return Err(StoreError::Unsupported(format!(
            "fs-file path '{}' is not a regular file",
            path.display()
        )));
    }

    let mode = metadata.permissions().mode() & 0o7777;
    let content_sha256 = sha256_file(path)?;
    hash_fs_file_parts(
        metadata.uid(),
        metadata.gid(),
        mode,
        metadata.size(),
        content_sha256,
    )
}

/// Computes an fs-file hash from already-collected file metadata and content
/// digest.
///
/// The hash algorithm is:
///
/// `sha256(b"bobr:fs-file:v1\0" || uid:u32be || gid:u32be || mode:u32be ||
/// size:u64be || sha256(file_bytes))`.
fn hash_fs_file_parts(
    uid: u32,
    gid: u32,
    mode: u32,
    size: u64,
    content_sha256: [u8; 32],
) -> Result<FsFileHash, StoreError> {
    validate_fs_file_mode(mode)?;

    let mut hasher = Sha256::new();
    hasher.update(FS_FILE_HASH_TAG);
    hasher.update(uid.to_be_bytes());
    hasher.update(gid.to_be_bytes());
    hasher.update(mode.to_be_bytes());
    hasher.update(size.to_be_bytes());
    hasher.update(content_sha256);

    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&hasher.finalize());
    Ok(FsFileHash(bytes))
}

fn scan_fs_tree_with_root(
    source_root: &Path,
    fs_files_root: &Path,
) -> Result<FsTreeManifest, StoreError> {
    require_existing_directory(source_root, "source root")?;
    require_existing_directory(fs_files_root, "fs-files root")?;

    let mut entries = Vec::new();
    scan_entry(source_root, source_root, fs_files_root, &mut entries)?;
    FsTreeManifest::from_entries(entries)
        .map_err(|error| StoreError::InvalidData(error.to_string()))
}

fn import_fs_tree_with_install_root(
    source_root: &Path,
    fs_files_root: &Path,
    install: &FsTreeInstall,
) -> Result<FsTreeManifest, StoreError> {
    require_existing_directory(source_root, "source root")?;
    require_existing_directory(fs_files_root, "fs-files root")?;
    let rules = compile_fs_tree_install_rules(install)?;

    let mut entries = Vec::new();
    import_installed_entry(
        source_root,
        source_root,
        fs_files_root,
        &rules,
        &mut entries,
    )?;
    FsTreeManifest::from_entries(entries)
        .map_err(|error| StoreError::InvalidData(error.to_string()))
}

fn materialize_fs_tree_with_root(
    manifest: &FsTreeManifest,
    fs_files_root: &Path,
    output_root: &Path,
) -> Result<(), StoreError> {
    require_existing_directory(fs_files_root, "fs-files root")?;
    require_existing_empty_directory(output_root, "output root")?;

    materialize_into_existing_root(manifest, fs_files_root, output_root)
}

fn require_absolute(path: &Path, label: &str) -> Result<(), StoreError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(StoreError::InvalidInput(format!(
            "{label} must be absolute: '{}'",
            path.display()
        )))
    }
}

fn require_existing_directory(path: &Path, label: &str) -> Result<(), StoreError> {
    require_absolute(path, label)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            StoreError::InvalidInput(format!("{label} must exist: '{}'", path.display()))
        } else {
            map_io(path, &format!("inspect {label}"), error)
        }
    })?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(StoreError::InvalidInput(format!(
            "{label} must be an existing real directory: '{}'",
            path.display()
        )))
    }
}

fn validate_fs_tree_root(root: &Path) -> Result<(), StoreError> {
    require_existing_directory(root, "store root")?;
    require_existing_directory(&root.join(FS_FILES_DIR), "store fs-files directory")?;
    require_existing_directory(&root.join(FS_TREES_DIR), "store fs-trees directory")?;
    Ok(())
}

fn require_existing_empty_directory(path: &Path, label: &str) -> Result<(), StoreError> {
    require_existing_directory(path, label)?;
    match fs::read_dir(path)
        .map_err(|error| map_io(path, &format!("read {label}"), error))?
        .next()
    {
        Some(Ok(entry)) => Err(StoreError::InvalidInput(format!(
            "{label} must be empty, found '{}'",
            entry.path().display()
        ))),
        Some(Err(error)) => Err(map_io(path, &format!("read {label} entry"), error)),
        None => Ok(()),
    }
}

fn validate_fs_file_mode(mode: u32) -> Result<(), StoreError> {
    validate_mode("fs-file mode", mode)
}

fn validate_mode(label: &str, mode: u32) -> Result<(), StoreError> {
    if mode <= 0o7777 {
        Ok(())
    } else {
        Err(StoreError::InvalidInput(format!(
            "{label} is out of range: {mode}"
        )))
    }
}

fn create_dir_all(path: &Path, label: &str) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|error| {
        StoreError::Io(format!(
            "failed to create {label} '{}': {error}",
            path.display()
        ))
    })
}

fn create_unique_fs_tree_staging_dir(
    fs_trees_root: &Path,
    manifest_hash: ObjectHash,
) -> Result<PathBuf, StoreError> {
    for attempt in 0..1024 {
        let path = fs_trees_root.join(format!(
            ".fs-tree-materialize-{}-{}-{attempt}",
            manifest_hash.to_hex(),
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(map_io(
                    &path,
                    "create fs-tree materialization staging directory",
                    error,
                ));
            }
        }
    }

    Err(StoreError::Io(format!(
        "failed to allocate fs-tree materialization staging directory under '{}'",
        fs_trees_root.display()
    )))
}

fn publish_materialized_fs_tree(staging_root: &Path, final_root: &Path) -> Result<(), StoreError> {
    fs::rename(staging_root, final_root).map_err(|error| {
        map_io(
            final_root,
            &format!(
                "publish fs-tree materialization from '{}'",
                staging_root.display()
            ),
            error,
        )
    })
}

fn map_io(path: &Path, action: &str, error: io::Error) -> StoreError {
    StoreError::Io(format!("failed to {action} '{}': {error}", path.display()))
}

fn scan_entry(
    scan_root: &Path,
    current_path: &Path,
    fs_files_root: &Path,
    entries: &mut Vec<FsTreeEntry>,
) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(current_path)
        .map_err(|error| map_io(current_path, "inspect", error))?;
    let file_type = metadata.file_type();
    let rel_path = manifest_relative_path(scan_root, current_path)?;
    if file_type.is_dir() {
        entries.push(FsTreeEntry::directory(
            rel_path,
            metadata.uid(),
            metadata.gid(),
            metadata.permissions().mode() & 0o7777,
        ));
        let mut children = fs::read_dir(current_path)
            .map_err(|error| map_io(current_path, "read directory", error))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| map_io(current_path, "read directory entry", error))?;
        children.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
        for child in children {
            scan_entry(scan_root, &child, fs_files_root, entries)?;
        }
    } else if file_type.is_symlink() {
        let target = fs::read_link(current_path)
            .map_err(|error| map_io(current_path, "read symlink", error))?;
        let target = target.to_str().ok_or_else(|| {
            StoreError::Unsupported(format!(
                "symlink target for '{}' is not UTF-8",
                current_path.display()
            ))
        })?;
        entries.push(FsTreeEntry::symlink(
            rel_path,
            metadata.uid(),
            metadata.gid(),
            target,
        ));
    } else if file_type.is_file() {
        let hash = hash_fs_file_path(current_path)?;
        let object_path = fs_file_path(fs_files_root, hash)?;
        match fs::symlink_metadata(&object_path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_fs_file_shard_dir(&object_path)?;
                match fs::hard_link(current_path, &object_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(map_io(
                            &object_path,
                            &format!("hardlink fs-file from '{}'", current_path.display()),
                            error,
                        ));
                    }
                }
            }
            Err(error) => return Err(map_io(&object_path, "inspect fs-file object", error)),
        }
        entries.push(FsTreeEntry::file(rel_path, hash));
    } else {
        return Err(StoreError::Unsupported(format!(
            "unsupported filesystem entry kind at '{}'",
            current_path.display()
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct CompiledFsTreeInstallRule {
    pattern: String,
    matcher: GlobMatcher,
    attrs: FsTreeInstallAttrs,
}

impl FsTreeInstallAttrs {
    fn overlay(&mut self, attrs: &Self) {
        if let Some(uid) = attrs.uid {
            self.uid = Some(uid);
        }
        if let Some(gid) = attrs.gid {
            self.gid = Some(gid);
        }
        if let Some(mode) = attrs.directory_mode {
            self.directory_mode = Some(mode);
        }
        if let Some(mode) = attrs.regular_file_mode {
            self.regular_file_mode = Some(mode);
        }
        if let Some(mode) = attrs.executable_file_mode {
            self.executable_file_mode = Some(mode);
        }
    }
}

fn compile_fs_tree_install_rules(
    install: &FsTreeInstall,
) -> Result<Vec<CompiledFsTreeInstallRule>, StoreError> {
    if install.rules.is_empty() {
        return Err(StoreError::InvalidInput(
            "fs-tree install rules must contain at least one rule".to_string(),
        ));
    }

    install
        .rules
        .iter()
        .map(|rule| {
            validate_install_rule_attrs(rule)?;
            let glob = Glob::new(&rule.path).map_err(|error| {
                StoreError::InvalidInput(format!(
                    "invalid fs-tree install rule pattern '{}': {error}",
                    rule.path
                ))
            })?;
            Ok(CompiledFsTreeInstallRule {
                pattern: rule.path.clone(),
                matcher: glob.compile_matcher(),
                attrs: rule.attrs.clone(),
            })
        })
        .collect()
}

#[derive(Debug)]
struct CompiledFsTreeSubsetPattern {
    pattern: String,
    matcher: GlobMatcher,
}

fn compile_fs_tree_subset_patterns(
    patterns: &[String],
) -> Result<Vec<CompiledFsTreeSubsetPattern>, StoreError> {
    if patterns.is_empty() {
        return Err(StoreError::InvalidInput(
            "fs-tree subset include patterns must contain at least one pattern".to_string(),
        ));
    }

    patterns
        .iter()
        .map(|pattern| {
            validate_fs_tree_subset_pattern(pattern)?;
            let glob = Glob::new(pattern).map_err(|error| {
                StoreError::InvalidInput(format!(
                    "invalid fs-tree subset include pattern '{}': {error}",
                    pattern
                ))
            })?;
            Ok(CompiledFsTreeSubsetPattern {
                pattern: pattern.clone(),
                matcher: glob.compile_matcher(),
            })
        })
        .collect()
}

fn validate_fs_tree_subset_pattern(pattern: &str) -> Result<(), StoreError> {
    if pattern.is_empty() {
        return Err(StoreError::InvalidInput(
            "fs-tree subset include pattern must not be empty".to_string(),
        ));
    }
    let path = Path::new(pattern);
    if path.is_absolute() {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree subset include pattern '{pattern}' must be relative"
        )));
    }
    if pattern.contains('\\') {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree subset include pattern '{pattern}' must use '/' separators"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree subset include pattern '{pattern}' must not contain '..'"
        )));
    }
    Ok(())
}

fn fs_tree_subset_path_matches(path: &str, patterns: &[CompiledFsTreeSubsetPattern]) -> bool {
    patterns.iter().any(|pattern| {
        pattern.matcher.is_match(path)
            || pattern
                .pattern
                .strip_suffix("/**")
                .is_some_and(|prefix| path == prefix)
    })
}

fn add_fs_tree_subset_parent_dirs(
    path: &str,
    by_path: &BTreeMap<&str, &FsTreeEntry>,
    selected: &mut BTreeSet<String>,
) -> Result<(), StoreError> {
    selected.insert(String::new());
    let mut remainder = path;
    while let Some((parent, _)) = remainder.rsplit_once('/') {
        let entry = by_path.get(parent).ok_or_else(|| {
            StoreError::InvalidData(format!(
                "fs-tree manifest is missing parent directory '{parent}' for '{path}'"
            ))
        })?;
        if !matches!(entry, FsTreeEntry::Directory { .. }) {
            return Err(StoreError::InvalidData(format!(
                "fs-tree manifest parent '{parent}' for '{path}' is not a directory"
            )));
        }
        selected.insert(parent.to_string());
        remainder = parent;
    }
    Ok(())
}

fn validate_install_rule_attrs(rule: &FsTreeInstallRule) -> Result<(), StoreError> {
    if let Some(mode) = rule.attrs.directory_mode {
        validate_mode(
            &format!("directory_mode in install rule '{}'", rule.path),
            mode,
        )?;
    }
    if let Some(mode) = rule.attrs.regular_file_mode {
        validate_mode(
            &format!("regular_file_mode in install rule '{}'", rule.path),
            mode,
        )?;
    }
    if let Some(mode) = rule.attrs.executable_file_mode {
        validate_mode(
            &format!("executable_file_mode in install rule '{}'", rule.path),
            mode,
        )?;
    }
    Ok(())
}

fn resolve_fs_tree_install_attrs(
    rel_path: &str,
    rules: &[CompiledFsTreeInstallRule],
) -> Result<FsTreeInstallAttrs, StoreError> {
    let mut resolved = FsTreeInstallAttrs::default();
    let mut matched_any = false;
    for rule in rules {
        if install_rule_matches(rule, rel_path) {
            matched_any = true;
            resolved.overlay(&rule.attrs);
        }
    }

    if matched_any {
        Ok(resolved)
    } else {
        let known = rules
            .iter()
            .map(|rule| rule.pattern.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(StoreError::InvalidInput(format!(
            "fs-tree path '{rel_path}' is not covered by any install rule (known patterns: {known})"
        )))
    }
}

fn install_rule_matches(rule: &CompiledFsTreeInstallRule, rel_path: &str) -> bool {
    if rule.matcher.is_match(rel_path) {
        return true;
    }

    if rel_path.is_empty() && rule.pattern == "**" {
        return true;
    }

    if let Some(prefix) = rule.pattern.strip_suffix("/**") {
        return rel_path == prefix;
    }

    false
}

fn required_install_attr(
    value: Option<u32>,
    rel_path: &str,
    name: &str,
) -> Result<u32, StoreError> {
    value.ok_or_else(|| {
        StoreError::InvalidInput(format!(
            "fs-tree path '{rel_path}' is missing resolved {name}"
        ))
    })
}

fn import_installed_entry(
    scan_root: &Path,
    current_path: &Path,
    fs_files_root: &Path,
    rules: &[CompiledFsTreeInstallRule],
    entries: &mut Vec<FsTreeEntry>,
) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(current_path)
        .map_err(|error| map_io(current_path, "inspect", error))?;
    let file_type = metadata.file_type();
    let rel_path = manifest_relative_path(scan_root, current_path)?;

    if !(file_type.is_dir() || file_type.is_symlink() || file_type.is_file()) {
        return Err(StoreError::Unsupported(format!(
            "unsupported filesystem entry kind at '{}'",
            current_path.display()
        )));
    }

    let attrs = resolve_fs_tree_install_attrs(&rel_path, rules)?;
    let uid = required_install_attr(attrs.uid, &rel_path, "uid")?;
    let gid = required_install_attr(attrs.gid, &rel_path, "gid")?;

    if file_type.is_dir() {
        let mode = required_install_attr(attrs.directory_mode, &rel_path, "directory_mode")?;
        entries.push(FsTreeEntry::directory(rel_path, uid, gid, mode));
        let mut children = fs::read_dir(current_path)
            .map_err(|error| map_io(current_path, "read directory", error))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| map_io(current_path, "read directory entry", error))?;
        children.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
        for child in children {
            import_installed_entry(scan_root, &child, fs_files_root, rules, entries)?;
        }
    } else if file_type.is_symlink() {
        let target = fs::read_link(current_path)
            .map_err(|error| map_io(current_path, "read symlink", error))?;
        let target = target.to_str().ok_or_else(|| {
            StoreError::Unsupported(format!(
                "symlink target for '{}' is not UTF-8",
                current_path.display()
            ))
        })?;
        entries.push(FsTreeEntry::symlink(rel_path, uid, gid, target));
    } else if file_type.is_file() {
        let source_mode = metadata.permissions().mode() & 0o7777;
        let mode = if source_mode & 0o111 != 0 {
            required_install_attr(
                attrs.executable_file_mode,
                &rel_path,
                "executable_file_mode",
            )?
        } else {
            required_install_attr(attrs.regular_file_mode, &rel_path, "regular_file_mode")?
        };
        let hash = import_fs_file_with_install(current_path, fs_files_root, uid, gid, mode)?;
        entries.push(FsTreeEntry::file(rel_path, hash));
    }
    Ok(())
}

fn import_fs_file_with_install(
    source_path: &Path,
    fs_files_root: &Path,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<FsFileHash, StoreError> {
    validate_fs_file_mode(mode)?;
    let temp_path = copy_source_file_to_fs_files_temp(source_path, fs_files_root)?;
    let result = publish_installed_temp_file(&temp_path, fs_files_root, uid, gid, mode);
    match result {
        Ok(hash) => {
            fs::remove_file(&temp_path)
                .map_err(|error| map_io(&temp_path, "remove temporary fs-file", error))?;
            Ok(hash)
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

fn publish_installed_temp_file(
    temp_path: &Path,
    fs_files_root: &Path,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<FsFileHash, StoreError> {
    chown_if_needed(temp_path, uid, gid)?;
    chmod(temp_path, mode)?;

    let metadata =
        fs::symlink_metadata(temp_path).map_err(|error| map_io(temp_path, "inspect", error))?;
    let actual_mode = metadata.permissions().mode() & 0o7777;
    if metadata.uid() != uid || metadata.gid() != gid || actual_mode != mode {
        return Err(StoreError::Io(format!(
            "temporary fs-file '{}' metadata mismatch after install: expected uid={uid} gid={gid} mode={mode:o}, got uid={} gid={} mode={actual_mode:o}",
            temp_path.display(),
            metadata.uid(),
            metadata.gid()
        )));
    }

    let content_sha256 = sha256_file(temp_path)?;
    let hash = hash_fs_file_parts(uid, gid, mode, metadata.size(), content_sha256)?;
    let object_path = fs_file_path(fs_files_root, hash)?;
    match fs::symlink_metadata(&object_path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_fs_file_shard_dir(&object_path)?;
            match fs::hard_link(temp_path, &object_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(map_io(
                        &object_path,
                        &format!("hardlink fs-file from '{}'", temp_path.display()),
                        error,
                    ));
                }
            }
        }
        Err(error) => return Err(map_io(&object_path, "inspect fs-file object", error)),
    }
    Ok(hash)
}

fn copy_source_file_to_fs_files_temp(
    source_path: &Path,
    fs_files_root: &Path,
) -> Result<PathBuf, StoreError> {
    require_absolute(source_path, "source file path")?;
    let mut source_file =
        fs::File::open(source_path).map_err(|error| map_io(source_path, "open file", error))?;
    let (temp_path, mut temp_file) = create_unique_fs_files_temp(fs_files_root)?;
    let copy_result = io::copy(&mut source_file, &mut temp_file)
        .map(|_| ())
        .map_err(|error| {
            map_io(
                &temp_path,
                &format!("copy fs-file from '{}'", source_path.display()),
                error,
            )
        });
    if let Err(error) = copy_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(temp_path)
}

fn create_unique_fs_files_temp(fs_files_root: &Path) -> Result<(PathBuf, fs::File), StoreError> {
    for attempt in 0..1024 {
        let temp_path =
            fs_files_root.join(format!(".fs-file-import-{}-{attempt}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(map_io(&temp_path, "create temporary fs-file", error)),
        }
    }

    Err(StoreError::Io(format!(
        "failed to allocate temporary fs-file under '{}'",
        fs_files_root.display()
    )))
}

fn create_fs_file_shard_dir(object_path: &Path) -> Result<(), StoreError> {
    let Some(shard_dir) = object_path.parent() else {
        return Err(StoreError::InvalidInput(format!(
            "fs-file object path has no parent directory: '{}'",
            object_path.display()
        )));
    };
    create_dir_all(shard_dir, "fs-files shard directory")
}
fn materialize_into_existing_root(
    manifest: &FsTreeManifest,
    fs_files_root: &Path,
    output_root: &Path,
) -> Result<(), StoreError> {
    for entry in manifest.entries() {
        match entry {
            FsTreeEntry::Directory { path, .. } if path.is_empty() => {}
            FsTreeEntry::Directory { path, .. } => {
                let dst = output_root.join(path);
                fs::create_dir(&dst).map_err(|error| map_io(&dst, "create directory", error))?;
            }
            FsTreeEntry::File { path, hash } => {
                let src = fs_file_path(fs_files_root, *hash)?;
                let dst = output_root.join(path);
                fs::hard_link(&src, &dst).map_err(|error| {
                    map_io(
                        &dst,
                        &format!("hardlink fs-tree file from '{}'", src.display()),
                        error,
                    )
                })?;
            }
            FsTreeEntry::Symlink {
                path,
                uid,
                gid,
                target,
            } => {
                let dst = output_root.join(path);
                symlink(target, &dst).map_err(|error| map_io(&dst, "create symlink", error))?;
                lchown_if_needed(&dst, *uid, *gid)?;
            }
        }
    }

    for entry in manifest.entries().iter().rev() {
        if let FsTreeEntry::Directory {
            path,
            uid,
            gid,
            mode,
        } = entry
        {
            let dst = if path.is_empty() {
                output_root.to_path_buf()
            } else {
                output_root.join(path)
            };
            chown_if_needed(&dst, *uid, *gid)?;
            chmod(&dst, *mode)?;
        }
    }

    Ok(())
}
fn manifest_relative_path(source_root: &Path, path: &Path) -> Result<String, StoreError> {
    let relative = path.strip_prefix(source_root).map_err(|error| {
        StoreError::InvalidInput(format!(
            "failed to resolve '{}' relative to '{}': {error}",
            path.display(),
            source_root.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(String::new());
    }
    relative.to_str().map(str::to_string).ok_or_else(|| {
        StoreError::Unsupported(format!("fs-tree path '{}' is not UTF-8", path.display()))
    })
}
fn sha256_file(path: &Path) -> Result<[u8; 32], StoreError> {
    let mut file = fs::File::open(path).map_err(|error| map_io(path, "open file", error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| map_io(path, "read file", error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&hasher.finalize());
    Ok(bytes)
}
fn chown_if_needed(path: &Path, uid: u32, gid: u32) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| map_io(path, "inspect", error))?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }
    chown(path, uid, gid)
}
fn lchown_if_needed(path: &Path, uid: u32, gid: u32) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| map_io(path, "inspect", error))?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }
    lchown(path, uid, gid)
}
fn chown(path: &Path, uid: u32, gid: u32) -> Result<(), StoreError> {
    let c_path = path_cstring(path)?;
    let result = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(map_io(path, "chown", io::Error::last_os_error()))
    }
}
fn lchown(path: &Path, uid: u32, gid: u32) -> Result<(), StoreError> {
    let c_path = path_cstring(path)?;
    let result = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(map_io(path, "lchown", io::Error::last_os_error()))
    }
}
fn path_cstring(path: &Path) -> Result<CString, StoreError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|error| {
        StoreError::InvalidInput(format!(
            "path contains NUL byte '{}': {error}",
            path.display()
        ))
    })
}
fn chmod(path: &Path, mode: u32) -> Result<(), StoreError> {
    validate_fs_file_mode(mode)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| map_io(path, "chmod", error))
}

fn validate_schema_header(raw_line: &str) -> Result<(), StoreError> {
    if raw_line.as_bytes() == CANONICAL_SCHEMA_LINE {
        return Ok(());
    }

    let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
    let value = serde_json::from_str::<Value>(line).map_err(|error| {
        StoreError::InvalidData(format!("failed to parse fs-tree schema header: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        StoreError::InvalidData("fs-tree schema header must be an object".to_string())
    })?;
    require_exact_keys(object, &["schema"], 1)?;
    let schema = required_string(object, "schema", 1)?;
    if schema != SCHEMA {
        return Err(StoreError::InvalidData(format!(
            "unsupported fs-tree manifest schema '{schema}'"
        )));
    }
    Err(StoreError::InvalidData(
        "fs-tree schema header is not canonical".to_string(),
    ))
}

fn validate_entries(entries: &[FsTreeEntry]) -> Result<(), StoreError> {
    if entries.is_empty() {
        return Err(StoreError::InvalidInput(
            "fs-tree manifest must contain at least the root directory".to_string(),
        ));
    }

    let mut previous_path: Option<&str> = None;
    let mut kinds = HashMap::with_capacity(entries.len());

    for entry in entries {
        validate_path(entry.path())?;
        validate_entry_strings(entry)?;
        validate_entry_numbers(entry)?;

        if let Some(previous_path) = previous_path
            && previous_path == entry.path()
        {
            return Err(StoreError::InvalidInput(format!(
                "duplicate fs-tree path '{}'",
                entry.path()
            )));
        }
        previous_path = Some(entry.path());
        kinds.insert(entry.path(), entry.kind());
    }

    match entries.first() {
        Some(FsTreeEntry::Directory { path, .. }) if path.is_empty() => {}
        Some(entry) if entry.path().is_empty() => {
            return Err(StoreError::InvalidInput(
                "fs-tree root path must be a directory".to_string(),
            ));
        }
        _ => {
            return Err(StoreError::InvalidInput(
                "fs-tree manifest must contain the root directory".to_string(),
            ));
        }
    }

    for entry in entries {
        let path = entry.path();
        if path.is_empty() {
            continue;
        }

        let parent = parent_path(path);
        match kinds.get(parent) {
            Some(FsTreeEntryKind::Directory) => {}
            Some(_) => {
                return Err(StoreError::InvalidInput(format!(
                    "parent '{}' for fs-tree path '{}' is not a directory",
                    parent, path
                )));
            }
            None => {
                return Err(StoreError::InvalidInput(format!(
                    "missing parent directory '{}' for fs-tree path '{}'",
                    parent, path
                )));
            }
        }
    }

    Ok(())
}

fn fs_tree_merge_conflict_reason(existing: &FsTreeEntry, new: &FsTreeEntry) -> String {
    if existing.kind() != new.kind() {
        return format!(
            "entry kind differs (existing {}, new {})",
            fs_tree_entry_kind_name(existing.kind()),
            fs_tree_entry_kind_name(new.kind())
        );
    }

    match (existing, new) {
        (
            FsTreeEntry::File {
                hash: existing_hash,
                ..
            },
            FsTreeEntry::File { hash: new_hash, .. },
        ) => format!("file hash differs ({existing_hash} vs {new_hash})"),
        (
            FsTreeEntry::Directory {
                uid: existing_uid,
                gid: existing_gid,
                mode: existing_mode,
                ..
            },
            FsTreeEntry::Directory {
                uid: new_uid,
                gid: new_gid,
                mode: new_mode,
                ..
            },
        ) => format!(
            "directory metadata differs ({}:{} {:o} vs {}:{} {:o})",
            existing_uid, existing_gid, existing_mode, new_uid, new_gid, new_mode
        ),
        (
            FsTreeEntry::Symlink {
                uid: existing_uid,
                gid: existing_gid,
                target: existing_target,
                ..
            },
            FsTreeEntry::Symlink {
                uid: new_uid,
                gid: new_gid,
                target: new_target,
                ..
            },
        ) => format!(
            "symlink metadata differs ({}:{} '{}' vs {}:{} '{}')",
            existing_uid, existing_gid, existing_target, new_uid, new_gid, new_target
        ),
        _ => "entry differs".to_string(),
    }
}

fn fs_tree_entry_kind_name(kind: FsTreeEntryKind) -> &'static str {
    match kind {
        FsTreeEntryKind::File => "file",
        FsTreeEntryKind::Directory => "directory",
        FsTreeEntryKind::Symlink => "symlink",
    }
}

fn validate_entry_strings(entry: &FsTreeEntry) -> Result<(), StoreError> {
    if let FsTreeEntry::Symlink { target, .. } = entry {
        validate_canonical_string("fs-tree symlink target", target)?;
    }
    Ok(())
}

fn validate_canonical_string(label: &str, value: &str) -> Result<(), StoreError> {
    if value.chars().any(char::is_control) {
        return Err(StoreError::InvalidInput(format!(
            "{label} contains a control character: '{}'",
            printable_path(value)
        )));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), StoreError> {
    if path.chars().any(char::is_control) {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree path contains a control character: '{}'",
            printable_path(path)
        )));
    }
    if path.is_empty() {
        return Ok(());
    }
    if path.starts_with('/') {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree path must be relative: '{path}'"
        )));
    }
    if path.ends_with('/') {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree path must not end with '/': '{path}'"
        )));
    }

    for component in path.split('/') {
        match component {
            "" => {
                return Err(StoreError::InvalidInput(format!(
                    "fs-tree path contains an empty component: '{path}'"
                )));
            }
            "." | ".." => {
                return Err(StoreError::InvalidInput(format!(
                    "fs-tree path contains forbidden component '{component}': '{path}'"
                )));
            }
            _ => {}
        }
    }

    Ok(())
}

fn validate_entry_numbers(entry: &FsTreeEntry) -> Result<(), StoreError> {
    let mode = match entry {
        FsTreeEntry::Directory { mode, .. } => *mode,
        FsTreeEntry::File { .. } | FsTreeEntry::Symlink { .. } => return Ok(()),
    };
    if mode > 0o7777 {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree mode for '{}' is out of range: {mode}",
            entry.path()
        )));
    }
    Ok(())
}

fn parent_path(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

fn parse_entry_line(line: &str, line_number: usize) -> Result<FsTreeEntry, StoreError> {
    let value = serde_json::from_str::<Value>(line).map_err(|error| {
        StoreError::InvalidData(format!("failed to parse line {line_number}: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        StoreError::InvalidData(format!(
            "fs-tree manifest line {line_number} is not an object"
        ))
    })?;

    let path = required_string(object, "p", line_number)?.to_string();
    let entry_type = required_string(object, "t", line_number)?;
    match entry_type {
        "f" => {
            require_exact_keys(object, &["p", "t", "h"], line_number)?;
            Ok(FsTreeEntry::File {
                path,
                hash: required_file_hash(object, "h", line_number)?,
            })
        }
        "d" => {
            require_exact_keys(object, &["p", "t", "u", "g", "m"], line_number)?;
            Ok(FsTreeEntry::Directory {
                path,
                uid: required_u32(object, "u", line_number)?,
                gid: required_u32(object, "g", line_number)?,
                mode: required_mode(object, "m", line_number)?,
            })
        }
        "l" => {
            require_exact_keys(object, &["p", "t", "u", "g", "x"], line_number)?;
            Ok(FsTreeEntry::Symlink {
                path,
                uid: required_u32(object, "u", line_number)?,
                gid: required_u32(object, "g", line_number)?,
                target: required_string(object, "x", line_number)?.to_string(),
            })
        }
        _ => Err(StoreError::InvalidData(format!(
            "invalid fs-tree entry type on line {line_number}: '{entry_type}'"
        ))),
    }
}

fn require_exact_keys(
    object: &Map<String, Value>,
    expected: &[&str],
    line_number: usize,
) -> Result<(), StoreError> {
    for key in expected {
        if !object.contains_key(*key) {
            return Err(StoreError::InvalidData(format!(
                "missing key '{key}' on fs-tree manifest line {line_number}"
            )));
        }
    }
    for key in object.keys() {
        if !expected.contains(&key.as_str()) {
            return Err(StoreError::InvalidData(format!(
                "unknown key '{key}' on fs-tree manifest line {line_number}"
            )));
        }
    }
    if object.len() != expected.len() {
        return Err(StoreError::InvalidData(format!(
            "invalid field set on fs-tree manifest line {line_number}"
        )));
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<&'a str, StoreError> {
    object.get(key).and_then(Value::as_str).ok_or_else(|| {
        StoreError::InvalidData(format!(
            "key '{key}' on fs-tree manifest line {line_number} must be a string"
        ))
    })
}

fn required_u32(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<u32, StoreError> {
    let raw = required_u64(object, key, line_number)?;
    u32::try_from(raw).map_err(|_| {
        StoreError::InvalidData(format!(
            "key '{key}' on fs-tree manifest line {line_number} is out of u32 range"
        ))
    })
}

fn required_mode(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<u32, StoreError> {
    let mode = required_u64(object, key, line_number)?;
    if mode > 0o7777 {
        return Err(StoreError::InvalidData(format!(
            "key '{key}' on fs-tree manifest line {line_number} is out of mode range"
        )));
    }
    Ok(mode as u32)
}

fn required_file_hash(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<FsFileHash, StoreError> {
    let raw = required_string(object, key, line_number)?;
    FsFileHash::from_str(raw).map_err(|error| {
        StoreError::InvalidData(format!(
            "key '{key}' on fs-tree manifest line {line_number} must be a lowercase fs-file hash: {error}"
        ))
    })
}

fn required_u64(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<u64, StoreError> {
    object.get(key).and_then(Value::as_u64).ok_or_else(|| {
        StoreError::InvalidData(format!(
            "key '{key}' on fs-tree manifest line {line_number} must be an unsigned integer"
        ))
    })
}

fn canonical_entry_bytes(entry: &FsTreeEntry) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    write_canonical_entry(entry, &mut out)?;
    Ok(out)
}

fn write_canonical_entry(entry: &FsTreeEntry, out: &mut Vec<u8>) -> Result<(), StoreError> {
    match entry {
        FsTreeEntry::File { path, hash } => {
            out.extend_from_slice(br#"{"p":""#);
            write_canonical_string(path, out)?;
            out.extend_from_slice(br#","t":"f","h":""#);
            out.extend_from_slice(hash.to_string().as_bytes());
            out.push(b'"');
            out.extend_from_slice(b"}\n");
        }
        FsTreeEntry::Directory {
            path,
            uid,
            gid,
            mode,
        } => {
            out.extend_from_slice(br#"{"p":""#);
            write_canonical_string(path, out)?;
            out.extend_from_slice(br#","t":"d","u":"#);
            write_u32(*uid, out);
            out.extend_from_slice(br#","g":"#);
            write_u32(*gid, out);
            out.extend_from_slice(br#","m":"#);
            write_u32(*mode, out);
            out.extend_from_slice(b"}\n");
        }
        FsTreeEntry::Symlink {
            path,
            uid,
            gid,
            target,
        } => {
            out.extend_from_slice(br#"{"p":""#);
            write_canonical_string(path, out)?;
            out.extend_from_slice(br#","t":"l","u":"#);
            write_u32(*uid, out);
            out.extend_from_slice(br#","g":"#);
            write_u32(*gid, out);
            out.extend_from_slice(br#","x":""#);
            write_canonical_string(target, out)?;
            out.extend_from_slice(b"}\n");
        }
    }
    Ok(())
}

fn write_canonical_string(value: &str, out: &mut Vec<u8>) -> Result<(), StoreError> {
    if value.chars().any(char::is_control) {
        return Err(StoreError::InvalidInput(format!(
            "fs-tree string contains a control character: '{}'",
            printable_path(value)
        )));
    }

    for byte in value.as_bytes() {
        match byte {
            b'"' => out.extend_from_slice(br#"\""#),
            b'\\' => out.extend_from_slice(br#"\\"#),
            _ => out.push(*byte),
        }
    }
    out.push(b'"');
    Ok(())
}

fn write_u32(value: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(value.to_string().as_bytes());
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn printable_path(path: &str) -> String {
    path.chars().flat_map(char::escape_default).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> FsFileHash {
        FsFileHash::from_str("1111111111111111111111111111111111111111111111111111111111111111")
            .unwrap()
    }

    fn other_hash() -> FsFileHash {
        FsFileHash::from_str("2222222222222222222222222222222222222222222222222222222222222222")
            .unwrap()
    }

    fn root() -> FsTreeEntry {
        FsTreeEntry::directory("", 0, 0, 0o755)
    }

    fn expected_installed_hash(uid: u32, gid: u32, mode: u32, content: &[u8]) -> FsFileHash {
        let mut content_hash = [0_u8; 32];
        content_hash.copy_from_slice(&Sha256::digest(content));
        hash_fs_file_parts(uid, gid, mode, content.len() as u64, content_hash).unwrap()
    }

    fn manifest(entries: Vec<FsTreeEntry>) -> FsTreeManifest {
        FsTreeManifest::from_entries(entries).expect("valid manifest")
    }

    fn assert_parse_rejects(input: &str) {
        assert!(
            FsTreeManifest::parse_canonical_bytes(input.as_bytes()).is_err(),
            "input was accepted: {input:?}"
        );
    }

    fn assert_line_rejects(line: &str) {
        assert_parse_rejects(&format!("{}\n{line}\n", schema_line()));
    }

    fn assert_entries_reject(entries: Vec<FsTreeEntry>) {
        assert!(
            FsTreeManifest::from_entries(entries).is_err(),
            "entries were accepted"
        );
    }

    fn schema_line() -> &'static str {
        r#"{"schema":"bobr-fs-tree-manifest"}"#
    }

    fn patterns(patterns: &[&str]) -> Vec<String> {
        patterns.iter().map(|pattern| pattern.to_string()).collect()
    }

    fn paths(manifest: &FsTreeManifest) -> Vec<&str> {
        manifest.entries().iter().map(FsTreeEntry::path).collect()
    }

    #[test]
    fn writer_emits_exact_canonical_jsonl() {
        let manifest = manifest(vec![
            root(),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file("bin/tool", hash()),
            FsTreeEntry::symlink("tool-link", 2, 3, "bin/tool"),
        ]);

        assert_eq!(
            String::from_utf8(manifest.to_canonical_bytes().unwrap()).unwrap(),
            concat!(
                r#"{"schema":"bobr-fs-tree-manifest"}"#,
                "\n",
                r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
                "\n",
                r#"{"p":"bin","t":"d","u":0,"g":0,"m":493}"#,
                "\n",
                r#"{"p":"bin/tool","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
                "\n",
                r#"{"p":"tool-link","t":"l","u":2,"g":3,"x":"bin/tool"}"#,
                "\n",
            )
        );
    }

    #[test]
    fn from_entries_sorts_by_path_bytes() {
        let manifest = manifest(vec![
            FsTreeEntry::file("b", hash()),
            FsTreeEntry::file("a", other_hash()),
            root(),
        ]);

        let paths: Vec<&str> = manifest.entries().iter().map(FsTreeEntry::path).collect();
        assert_eq!(paths, vec!["", "a", "b"]);
    }

    #[test]
    fn subset_manifest_selects_matches_and_parent_directories() {
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::directory("usr/bin", 0, 0, 0o755),
            FsTreeEntry::file("usr/bin/tool", hash()),
            FsTreeEntry::directory("usr/lib", 0, 0, 0o755),
            FsTreeEntry::file("usr/lib/libx.so", other_hash()),
            FsTreeEntry::symlink("tool-link", 1, 2, "usr/bin/tool"),
        ]);

        let subset = subset_manifest(&input, &patterns(&["usr/bin/*", "tool-link"])).unwrap();
        let entries = subset.entries();

        assert_eq!(
            paths(&subset),
            vec!["", "tool-link", "usr", "usr/bin", "usr/bin/tool"]
        );
        assert!(entries.contains(&FsTreeEntry::symlink("tool-link", 1, 2, "usr/bin/tool")));
        assert!(entries.contains(&FsTreeEntry::file("usr/bin/tool", hash())));
    }

    #[test]
    fn subset_manifest_double_star_matches_prefix_path() {
        let input = manifest(vec![
            root(),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::directory("usr/bin", 0, 0, 0o755),
            FsTreeEntry::file("usr/bin/tool", hash()),
            FsTreeEntry::file("usr/readme", other_hash()),
        ]);

        let subset = subset_manifest(&input, &patterns(&["usr/bin/**"])).unwrap();

        assert_eq!(paths(&subset), vec!["", "usr", "usr/bin", "usr/bin/tool"]);
    }

    #[test]
    fn subset_manifest_rejects_invalid_patterns_and_empty_result() {
        let input = manifest(vec![root(), FsTreeEntry::file("a", hash())]);

        for include in [
            Vec::<String>::new(),
            patterns(&[""]),
            patterns(&["/abs"]),
            patterns(&["a\\b"]),
            patterns(&["../a"]),
        ] {
            assert!(matches!(
                subset_manifest(&input, &include),
                Err(StoreError::InvalidInput(_))
            ));
        }

        assert!(matches!(
            subset_manifest(&input, &patterns(&["missing/**"])),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn merge_manifests_combines_disjoint_manifests() {
        let left = manifest(vec![
            root(),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::file("usr/tool", hash()),
        ]);
        let right = manifest(vec![
            root(),
            FsTreeEntry::directory("etc", 1, 2, 0o750),
            FsTreeEntry::symlink("etc/tool", 1, 2, "../usr/tool"),
        ]);

        let merged = merge_manifests(&[left, right]).unwrap();

        assert_eq!(
            paths(&merged),
            vec!["", "etc", "etc/tool", "usr", "usr/tool"]
        );
        assert!(
            merged
                .entries()
                .contains(&FsTreeEntry::directory("etc", 1, 2, 0o750))
        );
    }

    #[test]
    fn merge_manifests_allows_identical_overlaps() {
        let left = manifest(vec![
            root(),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file("bin/tool", hash()),
            FsTreeEntry::symlink("tool-link", 0, 0, "bin/tool"),
        ]);
        let right = left.clone();

        assert_eq!(merge_manifests(&[left.clone(), right]).unwrap(), left);
    }

    #[test]
    fn merge_manifests_rejects_conflicts_and_empty_input() {
        assert!(matches!(
            merge_manifests(&[]),
            Err(StoreError::InvalidInput(_))
        ));

        let base = manifest(vec![
            root(),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file("bin/tool", hash()),
            FsTreeEntry::symlink("tool-link", 0, 0, "bin/tool"),
        ]);

        for conflicting in [
            manifest(vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o700),
                FsTreeEntry::file("bin/tool", hash()),
                FsTreeEntry::symlink("tool-link", 0, 0, "bin/tool"),
            ]),
            manifest(vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o755),
                FsTreeEntry::file("bin/tool", other_hash()),
                FsTreeEntry::symlink("tool-link", 0, 0, "bin/tool"),
            ]),
            manifest(vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o755),
                FsTreeEntry::file("bin/tool", hash()),
                FsTreeEntry::symlink("tool-link", 0, 0, "bin/other"),
            ]),
            manifest(vec![
                root(),
                FsTreeEntry::directory("bin", 0, 0, 0o755),
                FsTreeEntry::directory("bin/tool", 0, 0, 0o755),
                FsTreeEntry::symlink("tool-link", 0, 0, "bin/tool"),
            ]),
        ] {
            assert!(matches!(
                merge_manifests(&[base.clone(), conflicting]),
                Err(StoreError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn parser_accepts_canonical_sample_and_round_trips() {
        let bytes = concat!(
            r#"{"schema":"bobr-fs-tree-manifest"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"a","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
        )
        .as_bytes();

        let manifest = FsTreeManifest::parse_canonical_bytes(bytes).unwrap();
        assert_eq!(manifest.to_canonical_bytes().unwrap(), bytes);
    }

    #[test]
    fn read_and_write_canonical_manifest() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("manifest.jsonl");
        let manifest = manifest(vec![root(), FsTreeEntry::file("a", hash())]);

        manifest.write_canonical(&path).unwrap();
        let read = FsTreeManifest::read_canonical(&path).unwrap();

        assert_eq!(read, manifest);
    }

    #[test]
    fn rejects_missing_final_newline_empty_file_and_empty_line() {
        assert_parse_rejects(r#"{"schema":"bobr-fs-tree-manifest"}"#);
        assert!(FsTreeManifest::parse_canonical_bytes(b"").is_err());
        assert_parse_rejects(concat!(r#"{"schema":"bobr-fs-tree-manifest"}"#, "\n", "\n",));
    }

    #[test]
    fn rejects_missing_wrong_or_non_canonical_schema_header() {
        assert_parse_rejects(concat!(r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#, "\n",));
        assert_parse_rejects(concat!(
            r#"{"schema":"wrong"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
        ));
        assert_parse_rejects(concat!(
            r#"{ "schema":"bobr-fs-tree-manifest"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
        ));
    }

    #[test]
    fn rejects_wrong_key_order_unknown_missing_and_duplicate_fields() {
        assert_line_rejects(r#"{"t":"d","p":"","u":0,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0,"m":493,"x":1}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0,"m":493,"u":0}"#);
    }

    #[test]
    fn rejects_file_metadata_and_symlink_hash() {
        assert_parse_rejects(concat!(
            r#"{"schema":"bobr-fs-tree-manifest"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"a","t":"f","u":0,"g":0,"m":420,"h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
        ));
        assert_parse_rejects(concat!(
            r#"{"schema":"bobr-fs-tree-manifest"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"l","t":"l","u":0,"g":0,"x":"target","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
        ));
    }

    #[test]
    fn rejects_duplicate_paths() {
        assert_entries_reject(vec![
            root(),
            FsTreeEntry::file("a", hash()),
            FsTreeEntry::directory("a", 0, 0, 0o755),
        ]);
        assert_parse_rejects(concat!(
            r#"{"schema":"bobr-fs-tree-manifest"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"a","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
            r#"{"p":"a","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
        ));
    }

    #[test]
    fn parser_rejects_entries_outside_canonical_order() {
        assert_parse_rejects(concat!(
            r#"{"schema":"bobr-fs-tree-manifest"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"b","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
            r#"{"p":"a","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
        ));
    }

    #[test]
    fn rejects_missing_root_non_directory_root_missing_parent_and_non_directory_parent() {
        assert_entries_reject(vec![FsTreeEntry::file("a", hash())]);
        assert_entries_reject(vec![FsTreeEntry::file("", hash())]);
        assert_entries_reject(vec![root(), FsTreeEntry::file("a/b", hash())]);
        assert_entries_reject(vec![
            root(),
            FsTreeEntry::file("a", hash()),
            FsTreeEntry::file("a/b", hash()),
        ]);
    }

    #[test]
    fn rejects_malformed_paths() {
        for path in ["/a", ".", "..", "a//b", "a/", "a/.", "a/..", "a\nb"] {
            assert_entries_reject(vec![root(), FsTreeEntry::file(path, hash())]);
        }
    }

    #[test]
    fn rejects_invalid_number_ranges_and_shapes() {
        assert_entries_reject(vec![root(), FsTreeEntry::directory("a", 0, 0, 0o10000)]);
        assert_line_rejects(r#"{"p":"","t":"d","u":4294967296,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":4294967296,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0,"m":4096}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":-1,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":1.0,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":null,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":[],"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":{},"m":493}"#);
    }

    #[test]
    fn writer_and_parser_handle_quote_and_backslash_paths() {
        let manifest = manifest(vec![
            root(),
            FsTreeEntry::file(r#"a"b"#, hash()),
            FsTreeEntry::file(r#"a\b"#, other_hash()),
        ]);
        let bytes = manifest.to_canonical_bytes().unwrap();

        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            concat!(
                r#"{"schema":"bobr-fs-tree-manifest"}"#,
                "\n",
                r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
                "\n",
                r#"{"p":"a\"b","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
                "\n",
                r#"{"p":"a\\b","t":"f","h":"2222222222222222222222222222222222222222222222222222222222222222"}"#,
                "\n",
            )
        );
        assert_eq!(
            FsTreeManifest::parse_canonical_bytes(&bytes).unwrap(),
            manifest
        );
    }

    #[test]
    fn parser_rejects_non_canonical_string_escapes_and_whitespace() {
        assert_line_rejects(
            r#"{"p":"a\/b","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
        );
        assert_line_rejects(
            r#"{"p":"\u0061","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
        );
        assert_line_rejects(r#"{ "p":"","t":"d","u":0,"g":0,"m":493}"#);
    }

    #[test]
    fn file_hash_parse_display_and_validate_lowercase_hex() {
        let hash = FsFileHash::from_str(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();

        assert_eq!(
            hash.to_string(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(hash.to_hex(), hash.to_string());
        assert!(FsFileHash::from_str("1234").is_err());
        assert!(
            FsFileHash::from_str(
                "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"
            )
            .is_err()
        );
        assert!(
            FsFileHash::from_str(
                "0123456789xyzdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )
            .is_err()
        );
    }

    #[test]
    fn fs_file_hash_vector_and_path_layout() {
        let mut content = [0_u8; 32];
        content.copy_from_slice(&Sha256::digest(b"hello\n"));
        let hash = hash_fs_file_parts(1, 2, 0o644, 6, content).unwrap();
        assert_eq!(
            hash.to_hex(),
            "cf253b6d1a9ce614d6887788ed05847b9c8398e82b6dd3f49b6be0fd7ad423ed"
        );

        let root = Path::new("/tmp/fs-files");
        assert_eq!(
            fs_file_path(root, hash).unwrap(),
            PathBuf::from(
                "/tmp/fs-files/cf/cf253b6d1a9ce614d6887788ed05847b9c8398e82b6dd3f49b6be0fd7ad423ed"
            )
        );
        assert!(hash_fs_file_parts(0, 0, 0o10000, 0, [0; 32]).is_err());
        assert!(fs_file_path(Path::new("relative"), hash).is_err());
    }

    #[test]
    fn existing_directory_checks_reject_missing_paths_and_symlinks() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().unwrap();
        let real = temp_dir.path().join("real");
        let link = temp_dir.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();

        assert!(require_existing_directory(&real, "test root").is_ok());
        assert!(matches!(
            require_existing_directory(&temp_dir.path().join("missing"), "test root"),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(matches!(
            require_existing_directory(&link, "test root"),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn scan_rejects_missing_fs_files_root() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        fs::create_dir(&source).unwrap();

        assert!(matches!(
            scan_fs_tree_with_root(&source, &fs_files),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn scan_creates_shards_lazily() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&fs_files).unwrap();
        fs::write(source.join("file"), b"data").unwrap();

        let hash = hash_fs_file_path(&source.join("file")).unwrap();
        let object_path = fs_file_path(&fs_files, hash).unwrap();
        assert!(!object_path.parent().unwrap().exists());

        scan_fs_tree_with_root(&source, &fs_files).unwrap();

        assert!(object_path.parent().unwrap().is_dir());
        assert!(object_path.is_file());
    }

    #[test]
    fn scan_imports_manifest_entries_and_reuses_fs_files() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        fs::create_dir(&fs_files).unwrap();
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(source.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(source.join("bin/tool"), b"tool\n").unwrap();
        fs::set_permissions(source.join("bin/tool"), fs::Permissions::from_mode(0o640)).unwrap();
        symlink("bin/tool", source.join("tool-link")).unwrap();

        let owner = fs::symlink_metadata(&source).unwrap();
        let manifest = scan_fs_tree_with_root(&source, &fs_files).unwrap();
        let file_hash = hash_fs_file_path(&source.join("bin/tool")).unwrap();
        let object_path = fs_file_path(&fs_files, file_hash).unwrap();

        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "",
            owner.uid(),
            owner.gid(),
            0o755
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "bin",
            owner.uid(),
            owner.gid(),
            0o700
        )));
        assert!(
            manifest
                .entries()
                .contains(&FsTreeEntry::file("bin/tool", file_hash))
        );
        assert!(manifest.entries().contains(&FsTreeEntry::symlink(
            "tool-link",
            owner.uid(),
            owner.gid(),
            "bin/tool"
        )));
        assert_eq!(
            fs::metadata(source.join("bin/tool")).unwrap().ino(),
            fs::metadata(&object_path).unwrap().ino()
        );

        let first_inode = fs::metadata(&object_path).unwrap().ino();
        let rescanned = scan_fs_tree_with_root(&source, &fs_files).unwrap();
        assert_eq!(rescanned, manifest);
        assert_eq!(fs::metadata(&object_path).unwrap().ino(), first_inode);
    }

    #[test]
    fn install_import_rejects_invalid_rules_uncovered_paths_and_missing_attrs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&fs_files).unwrap();

        assert!(matches!(
            import_fs_tree_with_install_root(
                &source,
                &fs_files,
                &FsTreeInstall { rules: Vec::new() },
            ),
            Err(StoreError::InvalidInput(_))
        ));

        let invalid_glob = FsTreeInstall {
            rules: vec![FsTreeInstallRule {
                path: "[".to_string(),
                attrs: FsTreeInstallAttrs::default(),
            }],
        };
        assert!(matches!(
            import_fs_tree_with_install_root(&source, &fs_files, &invalid_glob),
            Err(StoreError::InvalidInput(_))
        ));

        let uncovered = FsTreeInstall {
            rules: vec![FsTreeInstallRule {
                path: "bin/**".to_string(),
                attrs: FsTreeInstallAttrs {
                    uid: Some(0),
                    gid: Some(0),
                    directory_mode: Some(0o755),
                    regular_file_mode: Some(0o644),
                    executable_file_mode: Some(0o755),
                },
            }],
        };
        assert!(matches!(
            import_fs_tree_with_install_root(&source, &fs_files, &uncovered),
            Err(StoreError::InvalidInput(_))
        ));

        let missing_directory_mode = FsTreeInstall {
            rules: vec![FsTreeInstallRule {
                path: "**".to_string(),
                attrs: FsTreeInstallAttrs {
                    uid: Some(0),
                    gid: Some(0),
                    directory_mode: None,
                    regular_file_mode: Some(0o644),
                    executable_file_mode: Some(0o755),
                },
            }],
        };
        assert!(matches!(
            import_fs_tree_with_install_root(&source, &fs_files, &missing_directory_mode),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn install_import_rejects_missing_regular_and_executable_modes() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&fs_files).unwrap();
        fs::write(source.join("data"), b"data").unwrap();
        fs::write(source.join("tool"), b"tool").unwrap();
        fs::set_permissions(source.join("data"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(source.join("tool"), fs::Permissions::from_mode(0o700)).unwrap();
        let owner = fs::symlink_metadata(&source).unwrap();

        let missing_regular_mode = FsTreeInstall {
            rules: vec![FsTreeInstallRule {
                path: "**".to_string(),
                attrs: FsTreeInstallAttrs {
                    uid: Some(owner.uid()),
                    gid: Some(owner.gid()),
                    directory_mode: Some(0o755),
                    regular_file_mode: None,
                    executable_file_mode: Some(0o755),
                },
            }],
        };
        assert!(matches!(
            import_fs_tree_with_install_root(&source, &fs_files, &missing_regular_mode),
            Err(StoreError::InvalidInput(_))
        ));

        let missing_executable_mode = FsTreeInstall {
            rules: vec![FsTreeInstallRule {
                path: "**".to_string(),
                attrs: FsTreeInstallAttrs {
                    uid: Some(owner.uid()),
                    gid: Some(owner.gid()),
                    directory_mode: Some(0o755),
                    regular_file_mode: Some(0o644),
                    executable_file_mode: None,
                },
            }],
        };
        assert!(matches!(
            import_fs_tree_with_install_root(&source, &fs_files, &missing_executable_mode),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn install_import_applies_overlay_rules_and_preserves_source_metadata() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        fs::create_dir(&fs_files).unwrap();
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(source.join("bin"), fs::Permissions::from_mode(0o701)).unwrap();
        fs::write(source.join("bin/data"), b"data\n").unwrap();
        fs::write(source.join("bin/tool"), b"tool\n").unwrap();
        fs::set_permissions(source.join("bin/data"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(source.join("bin/tool"), fs::Permissions::from_mode(0o700)).unwrap();
        symlink("bin/tool", source.join("tool-link")).unwrap();

        let owner = fs::symlink_metadata(&source).unwrap();
        let install = FsTreeInstall {
            rules: vec![
                FsTreeInstallRule {
                    path: "**".to_string(),
                    attrs: FsTreeInstallAttrs {
                        uid: Some(owner.uid()),
                        gid: Some(owner.gid()),
                        directory_mode: Some(0o755),
                        regular_file_mode: Some(0o644),
                        executable_file_mode: Some(0o755),
                    },
                },
                FsTreeInstallRule {
                    path: "bin/**".to_string(),
                    attrs: FsTreeInstallAttrs {
                        uid: None,
                        gid: None,
                        directory_mode: Some(0o555),
                        regular_file_mode: Some(0o444),
                        executable_file_mode: None,
                    },
                },
                FsTreeInstallRule {
                    path: "bin/tool".to_string(),
                    attrs: FsTreeInstallAttrs {
                        uid: None,
                        gid: None,
                        directory_mode: None,
                        regular_file_mode: None,
                        executable_file_mode: Some(0o500),
                    },
                },
            ],
        };

        let manifest = import_fs_tree_with_install_root(&source, &fs_files, &install).unwrap();
        let data_hash = expected_installed_hash(owner.uid(), owner.gid(), 0o444, b"data\n");
        let tool_hash = expected_installed_hash(owner.uid(), owner.gid(), 0o500, b"tool\n");
        let data_object = fs_file_path(&fs_files, data_hash).unwrap();
        let tool_object = fs_file_path(&fs_files, tool_hash).unwrap();

        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "",
            owner.uid(),
            owner.gid(),
            0o755,
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::directory(
            "bin",
            owner.uid(),
            owner.gid(),
            0o555,
        )));
        assert!(
            manifest
                .entries()
                .contains(&FsTreeEntry::file("bin/data", data_hash))
        );
        assert!(
            manifest
                .entries()
                .contains(&FsTreeEntry::file("bin/tool", tool_hash))
        );
        assert!(manifest.entries().contains(&FsTreeEntry::symlink(
            "tool-link",
            owner.uid(),
            owner.gid(),
            "bin/tool",
        )));

        assert_eq!(fs::read(&data_object).unwrap(), b"data\n");
        assert_eq!(fs::read(&tool_object).unwrap(), b"tool\n");
        assert_eq!(
            fs::symlink_metadata(&data_object)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o444
        );
        assert_eq!(
            fs::symlink_metadata(&tool_object)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o500
        );
        assert_eq!(
            fs::symlink_metadata(source.join("bin/data"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        assert_eq!(
            fs::symlink_metadata(source.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );

        let data_inode = fs::metadata(&data_object).unwrap().ino();
        let tool_inode = fs::metadata(&tool_object).unwrap().ino();
        let second = import_fs_tree_with_install_root(&source, &fs_files, &install).unwrap();
        assert_eq!(second, manifest);
        assert_eq!(fs::metadata(&data_object).unwrap().ino(), data_inode);
        assert_eq!(fs::metadata(&tool_object).unwrap().ino(), tool_inode);
    }

    #[test]
    fn install_import_rejects_symlink_mode_field() {
        let value = serde_json::json!({
            "rules": [{
                "path": "**",
                "attrs": {
                    "uid": 0,
                    "gid": 0,
                    "directory_mode": 493,
                    "regular_file_mode": 420,
                    "executable_file_mode": 493,
                    "symlink_mode": 511
                }
            }]
        });

        assert!(serde_json::from_value::<FsTreeInstall>(value).is_err());
    }

    #[test]
    fn materialize_recreates_tree_and_hardlinks_fs_files() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        let output = temp_dir.path().join("output");
        fs::create_dir(&fs_files).unwrap();
        fs::create_dir(&output).unwrap();
        fs::create_dir_all(source.join("locked")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(source.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(source.join("locked/file"), b"data").unwrap();
        fs::set_permissions(
            source.join("locked/file"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        symlink("locked/file", source.join("link")).unwrap();

        let manifest = scan_fs_tree_with_root(&source, &fs_files).unwrap();
        let file_hash = hash_fs_file_path(&source.join("locked/file")).unwrap();
        let object_path = fs_file_path(&fs_files, file_hash).unwrap();

        materialize_fs_tree_with_root(&manifest, &fs_files, &output).unwrap();

        assert_eq!(fs::read(output.join("locked/file")).unwrap(), b"data");
        assert_eq!(
            fs::read_link(output.join("link")).unwrap(),
            PathBuf::from("locked/file")
        );
        assert_eq!(
            fs::metadata(output.join("locked/file")).unwrap().ino(),
            fs::metadata(&object_path).unwrap().ino()
        );
        assert_eq!(
            fs::symlink_metadata(output.join("locked"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            fs::symlink_metadata(&output).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn materialize_applies_restrictive_directory_modes_after_children() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp_dir = tempfile::tempdir().unwrap();
        let fs_files = temp_dir.path().join("fs-files");
        let source_file = temp_dir.path().join("source-file");
        let output = temp_dir.path().join("output");
        fs::create_dir(&fs_files).unwrap();
        fs::create_dir(&output).unwrap();
        fs::write(&source_file, b"secret").unwrap();
        fs::set_permissions(&source_file, fs::Permissions::from_mode(0o600)).unwrap();
        let source_meta = fs::symlink_metadata(&source_file).unwrap();
        let hash = hash_fs_file_path(&source_file).unwrap();
        let object_path = fs_file_path(&fs_files, hash).unwrap();
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::hard_link(&source_file, object_path).unwrap();
        let manifest = manifest(vec![
            FsTreeEntry::directory("", source_meta.uid(), source_meta.gid(), 0o755),
            FsTreeEntry::directory("locked", source_meta.uid(), source_meta.gid(), 0o000),
            FsTreeEntry::file("locked/file", hash),
        ]);

        materialize_fs_tree_with_root(&manifest, &fs_files, &output).unwrap();

        assert_eq!(fs::metadata(&source_file).unwrap().nlink(), 3);
        assert_eq!(
            fs::symlink_metadata(output.join("locked"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o000
        );
        fs::set_permissions(output.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn scan_rejects_relative_root_root_symlink_non_utf8_and_special_files() {
        use std::ffi::CString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let fs_files = temp_dir.path().join("fs-files");
        fs::create_dir(&source).unwrap();

        assert!(matches!(
            scan_fs_tree_with_root(Path::new("relative"), &fs_files),
            Err(StoreError::InvalidInput(_))
        ));

        let root_link = temp_dir.path().join("source-link");
        symlink(&source, &root_link).unwrap();
        assert!(matches!(
            scan_fs_tree_with_root(&root_link, &fs_files),
            Err(StoreError::InvalidInput(_))
        ));

        fs::create_dir(&fs_files).unwrap();

        let non_utf8_name = std::ffi::OsString::from_vec(vec![b'b', 0xff]);
        fs::write(source.join(non_utf8_name), b"x").unwrap();
        assert!(matches!(
            scan_fs_tree_with_root(&source, &fs_files),
            Err(StoreError::Unsupported(_))
        ));
        fs::remove_file(source.join(std::ffi::OsString::from_vec(vec![b'b', 0xff]))).unwrap();

        symlink(
            PathBuf::from(std::ffi::OsString::from_vec(vec![b't', 0xff])),
            source.join("bad-link"),
        )
        .unwrap();
        assert!(matches!(
            scan_fs_tree_with_root(&source, &fs_files),
            Err(StoreError::Unsupported(_))
        ));
        fs::remove_file(source.join("bad-link")).unwrap();

        let fifo = source.join("fifo");
        let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(result, 0);
        assert!(matches!(
            scan_fs_tree_with_root(&source, &fs_files),
            Err(StoreError::Unsupported(_))
        ));
    }

    #[test]
    fn materialize_rejects_missing_non_empty_and_symlink_output_roots() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let temp_dir = tempfile::tempdir().unwrap();
        let fs_files = temp_dir.path().join("fs-files");
        let missing_output = temp_dir.path().join("missing-output");
        let non_empty_output = temp_dir.path().join("non-empty-output");
        let real_output = temp_dir.path().join("real-output");
        let symlink_output = temp_dir.path().join("symlink-output");
        fs::create_dir(&fs_files).unwrap();
        fs::create_dir(&non_empty_output).unwrap();
        fs::write(non_empty_output.join("existing"), b"x").unwrap();
        fs::create_dir(&real_output).unwrap();
        symlink(&real_output, &symlink_output).unwrap();
        let owner = fs::symlink_metadata(temp_dir.path()).unwrap();
        let manifest = manifest(vec![FsTreeEntry::directory(
            "",
            owner.uid(),
            owner.gid(),
            0o755,
        )]);

        assert!(matches!(
            materialize_fs_tree_with_root(&manifest, &fs_files, &missing_output),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(matches!(
            materialize_fs_tree_with_root(&manifest, &fs_files, &non_empty_output),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(matches!(
            materialize_fs_tree_with_root(&manifest, &fs_files, &symlink_output),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn materialize_keeps_partial_output_on_failure() {
        use std::os::unix::fs::MetadataExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let fs_files = temp_dir.path().join("fs-files");
        let output = temp_dir.path().join("output");
        fs::create_dir(&fs_files).unwrap();
        fs::create_dir(&output).unwrap();
        let owner = fs::symlink_metadata(temp_dir.path()).unwrap();
        let missing_hash = hash();
        let manifest = manifest(vec![
            FsTreeEntry::directory("", owner.uid(), owner.gid(), 0o755),
            FsTreeEntry::directory("partial", owner.uid(), owner.gid(), 0o755),
            FsTreeEntry::file("partial/missing", missing_hash),
        ]);

        assert!(materialize_fs_tree_with_root(&manifest, &fs_files, &output).is_err());
        assert!(output.exists());
        assert!(output.join("partial").is_dir());
    }

    #[test]
    fn ensure_materialized_root_creates_fs_tree_cache_from_manifest_object() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store_root = temp_dir.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = crate::Store::create(&store_root).unwrap();
        let source = temp_dir.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"hello\n").unwrap();
        let manifest = store.fs_tree().scan(&source).unwrap();
        let staged_manifest = temp_dir.path().join("manifest.jsonl");
        manifest.write_canonical(&staged_manifest).unwrap();
        let manifest_hash = crate::import_object(&store, &staged_manifest).unwrap();

        let root = store
            .fs_tree()
            .ensure_materialized_root(manifest_hash)
            .unwrap();

        assert_eq!(
            root,
            store.root().join(FS_TREES_DIR).join(manifest_hash.to_hex())
        );
        assert_eq!(fs::read(root.join("file")).unwrap(), b"hello\n");
        assert_eq!(
            store
                .fs_tree()
                .lookup_materialized_root(manifest_hash)
                .unwrap(),
            Some(root)
        );
    }

    #[test]
    fn ensure_materialized_root_reuses_existing_cache_without_manifest_object() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store_root = temp_dir.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = crate::Store::create(&store_root).unwrap();
        let manifest_hash = ObjectHash::from_str(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let root = store.root().join(FS_TREES_DIR).join(manifest_hash.to_hex());
        fs::create_dir(&root).unwrap();

        assert_eq!(
            store
                .fs_tree()
                .ensure_materialized_root(manifest_hash)
                .unwrap(),
            root
        );
    }

    #[test]
    fn lookup_materialized_root_rejects_non_directory_cache_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store_root = temp_dir.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = crate::Store::create(&store_root).unwrap();
        let manifest_hash = ObjectHash::from_str(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        fs::write(
            store.root().join(FS_TREES_DIR).join(manifest_hash.to_hex()),
            b"not a dir",
        )
        .unwrap();

        assert!(matches!(
            store.fs_tree().lookup_materialized_root(manifest_hash),
            Err(StoreError::InvalidData(_))
        ));
    }

    #[test]
    fn ensure_materialized_root_does_not_publish_cache_on_missing_fs_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store_root = temp_dir.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = crate::Store::create(&store_root).unwrap();
        let manifest = manifest(vec![root(), FsTreeEntry::file("missing", hash())]);
        let staged_manifest = temp_dir.path().join("manifest.jsonl");
        manifest.write_canonical(&staged_manifest).unwrap();
        let manifest_hash = crate::import_object(&store, &staged_manifest).unwrap();

        assert!(
            store
                .fs_tree()
                .ensure_materialized_root(manifest_hash)
                .is_err()
        );
        assert!(
            !store
                .root()
                .join(FS_TREES_DIR)
                .join(manifest_hash.to_hex())
                .exists()
        );
    }
}
