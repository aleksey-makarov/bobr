//! Canonical `fs-tree manifest v2` parser, writer, and validation types.
//!
//! This crate owns the future manifest-addressed fs-tree manifest format. It
//! does not implement fs-file storage, fs-file hashing, tree scanning,
//! materialization, or builder integration.

#![deny(missing_docs)]

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

const SCHEMA: &str = "mbuild-fs-tree-manifest-v2";
const CANONICAL_SCHEMA_LINE: &[u8] = br#"{"schema":"mbuild-fs-tree-manifest-v2"}
"#;

/// Canonical `fs-tree manifest v2`.
///
/// A manifest contains validated filesystem entries sorted by UTF-8 path bytes.
/// The serialized form always starts with the
/// `{"schema":"mbuild-fs-tree-manifest-v2"}` header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeManifest {
    entries: Vec<FsTreeEntry>,
}

/// One filesystem entry in an `fs-tree manifest v2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeEntry {
    /// Regular file entry.
    ///
    /// The entry stores only the path and an opaque future fs-file object hash.
    /// Regular file bytes, owner, group, and mode are intentionally not stored
    /// in manifest v2.
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

/// Opaque future fs-file object hash used by manifest v2 regular file entries.
///
/// The type validates and formats exactly 64 lowercase hex digits. The hash
/// algorithm is deliberately not defined in this crate yet.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FsFileHash([u8; 32]);

/// Error returned by manifest parsing, validation, and file I/O operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeManifestError {
    /// Entry values or tree shape are invalid.
    Invalid(String),
    /// Canonical manifest bytes are malformed or non-canonical.
    Parse(String),
    /// Reading or writing a manifest failed.
    Io(String),
}

impl fmt::Display for FsTreeManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Parse(message) | Self::Io(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for FsTreeManifestError {}

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

impl FsTreeManifest {
    /// Builds a manifest from entries.
    ///
    /// Entries are sorted by path bytes before validation. The resulting
    /// manifest must contain a root directory entry, must not contain duplicate
    /// paths, and every non-root entry must have an explicit directory parent.
    pub fn from_entries(mut entries: Vec<FsTreeEntry>) -> Result<Self, FsTreeManifestError> {
        entries.sort_by(|left, right| left.path().as_bytes().cmp(right.path().as_bytes()));
        validate_entries(&entries)?;
        Ok(Self { entries })
    }

    /// Parses canonical manifest v2 bytes.
    ///
    /// This accepts only the exact canonical encoding produced by
    /// [`FsTreeManifest::to_canonical_bytes`]. Non-canonical JSON whitespace,
    /// alternate field order, missing final newline, unknown fields, duplicate
    /// paths, malformed paths, and missing parent directories are rejected.
    pub fn parse_canonical_bytes(bytes: &[u8]) -> Result<Self, FsTreeManifestError> {
        if bytes.is_empty() {
            return Err(FsTreeManifestError::Parse(
                "fs-tree manifest must not be empty".to_string(),
            ));
        }
        if !bytes.ends_with(b"\n") {
            return Err(FsTreeManifestError::Parse(
                "fs-tree manifest must end with a newline".to_string(),
            ));
        }

        let text = std::str::from_utf8(bytes).map_err(|error| {
            FsTreeManifestError::Parse(format!("fs-tree manifest is not UTF-8: {error}"))
        })?;

        let mut lines = text.split_inclusive('\n');
        let Some(schema_line) = lines.next() else {
            return Err(FsTreeManifestError::Parse(
                "fs-tree manifest is missing schema header".to_string(),
            ));
        };
        validate_schema_header(schema_line)?;

        let mut entries = Vec::new();
        for (index, raw_line) in lines.enumerate() {
            let line_number = index + 2;
            let line = raw_line.strip_suffix('\n').expect("split line has suffix");
            if line.is_empty() {
                return Err(FsTreeManifestError::Parse(format!(
                    "fs-tree manifest line {line_number} is empty"
                )));
            }

            let entry = parse_entry_line(line, line_number)?;
            let canonical = canonical_entry_bytes(&entry)?;
            if canonical.as_slice() != raw_line.as_bytes() {
                return Err(FsTreeManifestError::Parse(format!(
                    "fs-tree manifest line {line_number} is not canonical"
                )));
            }
            entries.push(entry);
        }

        let manifest = Self::from_entries(entries)?;
        let canonical = manifest.to_canonical_bytes()?;
        if canonical.as_slice() != bytes {
            return Err(FsTreeManifestError::Parse(
                "fs-tree manifest entries are not in canonical order".to_string(),
            ));
        }
        Ok(manifest)
    }

    /// Reads and parses a canonical manifest v2 file.
    pub fn read_canonical(path: &Path) -> Result<Self, FsTreeManifestError> {
        let bytes = fs::read(path).map_err(|error| {
            FsTreeManifestError::Io(format!(
                "failed to read fs-tree manifest '{}': {error}",
                path.display()
            ))
        })?;
        Self::parse_canonical_bytes(&bytes)
    }

    /// Serializes this manifest to canonical JSONL bytes.
    ///
    /// The output always includes the schema header and a final newline.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, FsTreeManifestError> {
        validate_entries(&self.entries)?;
        let mut out = Vec::new();
        out.extend_from_slice(CANONICAL_SCHEMA_LINE);
        for entry in &self.entries {
            write_canonical_entry(entry, &mut out)?;
        }
        Ok(out)
    }

    /// Writes this manifest as canonical JSONL bytes.
    pub fn write_canonical(&self, path: &Path) -> Result<(), FsTreeManifestError> {
        let bytes = self.to_canonical_bytes()?;
        fs::write(path, bytes).map_err(|error| {
            FsTreeManifestError::Io(format!(
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

fn validate_schema_header(raw_line: &str) -> Result<(), FsTreeManifestError> {
    if raw_line.as_bytes() == CANONICAL_SCHEMA_LINE {
        return Ok(());
    }

    let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
    let value = serde_json::from_str::<Value>(line).map_err(|error| {
        FsTreeManifestError::Parse(format!("failed to parse fs-tree schema header: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        FsTreeManifestError::Parse("fs-tree schema header must be an object".to_string())
    })?;
    require_exact_keys(object, &["schema"], 1)?;
    let schema = required_string(object, "schema", 1)?;
    if schema != SCHEMA {
        return Err(FsTreeManifestError::Parse(format!(
            "unsupported fs-tree manifest schema '{schema}'"
        )));
    }
    Err(FsTreeManifestError::Parse(
        "fs-tree schema header is not canonical".to_string(),
    ))
}

fn validate_entries(entries: &[FsTreeEntry]) -> Result<(), FsTreeManifestError> {
    if entries.is_empty() {
        return Err(FsTreeManifestError::Invalid(
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
            return Err(FsTreeManifestError::Invalid(format!(
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
            return Err(FsTreeManifestError::Invalid(
                "fs-tree root path must be a directory".to_string(),
            ));
        }
        _ => {
            return Err(FsTreeManifestError::Invalid(
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
                return Err(FsTreeManifestError::Invalid(format!(
                    "parent '{}' for fs-tree path '{}' is not a directory",
                    parent, path
                )));
            }
            None => {
                return Err(FsTreeManifestError::Invalid(format!(
                    "missing parent directory '{}' for fs-tree path '{}'",
                    parent, path
                )));
            }
        }
    }

    Ok(())
}

fn validate_entry_strings(entry: &FsTreeEntry) -> Result<(), FsTreeManifestError> {
    if let FsTreeEntry::Symlink { target, .. } = entry {
        validate_canonical_string("fs-tree symlink target", target)?;
    }
    Ok(())
}

fn validate_canonical_string(label: &str, value: &str) -> Result<(), FsTreeManifestError> {
    if value.chars().any(char::is_control) {
        return Err(FsTreeManifestError::Invalid(format!(
            "{label} contains a control character: '{}'",
            printable_path(value)
        )));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), FsTreeManifestError> {
    if path.chars().any(char::is_control) {
        return Err(FsTreeManifestError::Invalid(format!(
            "fs-tree path contains a control character: '{}'",
            printable_path(path)
        )));
    }
    if path.is_empty() {
        return Ok(());
    }
    if path.starts_with('/') {
        return Err(FsTreeManifestError::Invalid(format!(
            "fs-tree path must be relative: '{path}'"
        )));
    }
    if path.ends_with('/') {
        return Err(FsTreeManifestError::Invalid(format!(
            "fs-tree path must not end with '/': '{path}'"
        )));
    }

    for component in path.split('/') {
        match component {
            "" => {
                return Err(FsTreeManifestError::Invalid(format!(
                    "fs-tree path contains an empty component: '{path}'"
                )));
            }
            "." | ".." => {
                return Err(FsTreeManifestError::Invalid(format!(
                    "fs-tree path contains forbidden component '{component}': '{path}'"
                )));
            }
            _ => {}
        }
    }

    Ok(())
}

fn validate_entry_numbers(entry: &FsTreeEntry) -> Result<(), FsTreeManifestError> {
    let mode = match entry {
        FsTreeEntry::Directory { mode, .. } => *mode,
        FsTreeEntry::File { .. } | FsTreeEntry::Symlink { .. } => return Ok(()),
    };
    if mode > 0o7777 {
        return Err(FsTreeManifestError::Invalid(format!(
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

fn parse_entry_line(line: &str, line_number: usize) -> Result<FsTreeEntry, FsTreeManifestError> {
    let value = serde_json::from_str::<Value>(line).map_err(|error| {
        FsTreeManifestError::Parse(format!("failed to parse line {line_number}: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        FsTreeManifestError::Parse(format!(
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
        _ => Err(FsTreeManifestError::Parse(format!(
            "invalid fs-tree entry type on line {line_number}: '{entry_type}'"
        ))),
    }
}

fn require_exact_keys(
    object: &Map<String, Value>,
    expected: &[&str],
    line_number: usize,
) -> Result<(), FsTreeManifestError> {
    for key in expected {
        if !object.contains_key(*key) {
            return Err(FsTreeManifestError::Parse(format!(
                "missing key '{key}' on fs-tree manifest line {line_number}"
            )));
        }
    }
    for key in object.keys() {
        if !expected.contains(&key.as_str()) {
            return Err(FsTreeManifestError::Parse(format!(
                "unknown key '{key}' on fs-tree manifest line {line_number}"
            )));
        }
    }
    if object.len() != expected.len() {
        return Err(FsTreeManifestError::Parse(format!(
            "invalid field set on fs-tree manifest line {line_number}"
        )));
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<&'a str, FsTreeManifestError> {
    object.get(key).and_then(Value::as_str).ok_or_else(|| {
        FsTreeManifestError::Parse(format!(
            "key '{key}' on fs-tree manifest line {line_number} must be a string"
        ))
    })
}

fn required_u32(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<u32, FsTreeManifestError> {
    let raw = required_u64(object, key, line_number)?;
    u32::try_from(raw).map_err(|_| {
        FsTreeManifestError::Parse(format!(
            "key '{key}' on fs-tree manifest line {line_number} is out of u32 range"
        ))
    })
}

fn required_mode(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<u32, FsTreeManifestError> {
    let mode = required_u64(object, key, line_number)?;
    if mode > 0o7777 {
        return Err(FsTreeManifestError::Parse(format!(
            "key '{key}' on fs-tree manifest line {line_number} is out of mode range"
        )));
    }
    Ok(mode as u32)
}

fn required_file_hash(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<FsFileHash, FsTreeManifestError> {
    let raw = required_string(object, key, line_number)?;
    FsFileHash::from_str(raw).map_err(|error| {
        FsTreeManifestError::Parse(format!(
            "key '{key}' on fs-tree manifest line {line_number} must be a lowercase fs-file hash: {error}"
        ))
    })
}

fn required_u64(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<u64, FsTreeManifestError> {
    object.get(key).and_then(Value::as_u64).ok_or_else(|| {
        FsTreeManifestError::Parse(format!(
            "key '{key}' on fs-tree manifest line {line_number} must be an unsigned integer"
        ))
    })
}

fn canonical_entry_bytes(entry: &FsTreeEntry) -> Result<Vec<u8>, FsTreeManifestError> {
    let mut out = Vec::new();
    write_canonical_entry(entry, &mut out)?;
    Ok(out)
}

fn write_canonical_entry(
    entry: &FsTreeEntry,
    out: &mut Vec<u8>,
) -> Result<(), FsTreeManifestError> {
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

fn write_canonical_string(value: &str, out: &mut Vec<u8>) -> Result<(), FsTreeManifestError> {
    if value.chars().any(char::is_control) {
        return Err(FsTreeManifestError::Invalid(format!(
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
        r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#
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
                r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
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
    fn parser_accepts_canonical_sample_and_round_trips() {
        let bytes = concat!(
            r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
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
        assert_parse_rejects(r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#);
        assert!(FsTreeManifest::parse_canonical_bytes(b"").is_err());
        assert_parse_rejects(concat!(
            r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
            "\n",
            "\n",
        ));
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
            r#"{ "schema":"mbuild-fs-tree-manifest-v2"}"#,
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
            r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
            "\n",
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"a","t":"f","u":0,"g":0,"m":420,"h":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            "\n",
        ));
        assert_parse_rejects(concat!(
            r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
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
            r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
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
            r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
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
                r#"{"schema":"mbuild-fs-tree-manifest-v2"}"#,
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
}
