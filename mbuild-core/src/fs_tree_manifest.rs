use fsobj_hash::ObjectHash;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTreeManifest {
    entries: Vec<FsTreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeEntry {
    File {
        path: String,
        uid: u32,
        gid: u32,
        mode: u32,
        hash: ObjectHash,
    },
    Directory {
        path: String,
        uid: u32,
        gid: u32,
        mode: u32,
    },
    Symlink {
        path: String,
        uid: u32,
        gid: u32,
        target: String,
        hash: ObjectHash,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsTreeManifestError {
    Invalid(String),
    Parse(String),
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

impl FsTreeManifest {
    pub fn from_entries(mut entries: Vec<FsTreeEntry>) -> Result<Self, FsTreeManifestError> {
        entries.sort_by(|left, right| left.path().as_bytes().cmp(right.path().as_bytes()));
        validate_entries(&entries)?;
        Ok(Self { entries })
    }

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

        let mut entries = Vec::new();
        for (index, raw_line) in text.split_inclusive('\n').enumerate() {
            let line_number = index + 1;
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

    pub fn read_canonical(path: &Path) -> Result<Self, FsTreeManifestError> {
        let bytes = fs::read(path).map_err(|error| {
            FsTreeManifestError::Io(format!(
                "failed to read fs-tree manifest '{}': {error}",
                path.display()
            ))
        })?;
        Self::parse_canonical_bytes(&bytes)
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, FsTreeManifestError> {
        validate_entries(&self.entries)?;
        let mut out = Vec::new();
        for entry in &self.entries {
            write_canonical_entry(entry, &mut out)?;
        }
        Ok(out)
    }

    pub fn write_canonical(&self, path: &Path) -> Result<(), FsTreeManifestError> {
        let bytes = self.to_canonical_bytes()?;
        fs::write(path, bytes).map_err(|error| {
            FsTreeManifestError::Io(format!(
                "failed to write fs-tree manifest '{}': {error}",
                path.display()
            ))
        })
    }

    pub fn entries(&self) -> &[FsTreeEntry] {
        &self.entries
    }
}

impl FsTreeEntry {
    pub fn file(path: impl Into<String>, uid: u32, gid: u32, mode: u32) -> Self {
        Self::File {
            path: path.into(),
            uid,
            gid,
            mode,
            hash: placeholder_leaf_hash(),
        }
    }

    pub fn file_with_hash(
        path: impl Into<String>,
        uid: u32,
        gid: u32,
        mode: u32,
        hash: ObjectHash,
    ) -> Self {
        Self::File {
            path: path.into(),
            uid,
            gid,
            mode,
            hash,
        }
    }

    pub fn directory(path: impl Into<String>, uid: u32, gid: u32, mode: u32) -> Self {
        Self::Directory {
            path: path.into(),
            uid,
            gid,
            mode,
        }
    }

    pub fn symlink(path: impl Into<String>, uid: u32, gid: u32, target: impl Into<String>) -> Self {
        Self::Symlink {
            path: path.into(),
            uid,
            gid,
            target: target.into(),
            hash: placeholder_leaf_hash(),
        }
    }

    pub fn symlink_with_hash(
        path: impl Into<String>,
        uid: u32,
        gid: u32,
        target: impl Into<String>,
        hash: ObjectHash,
    ) -> Self {
        Self::Symlink {
            path: path.into(),
            uid,
            gid,
            target: target.into(),
            hash,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::File { path, .. } | Self::Directory { path, .. } | Self::Symlink { path, .. } => {
                path
            }
        }
    }

    fn kind(&self) -> EntryKind {
        match self {
            Self::File { .. } => EntryKind::File,
            Self::Directory { .. } => EntryKind::Directory,
            Self::Symlink { .. } => EntryKind::Symlink,
        }
    }

    pub fn leaf_hash(&self) -> Option<ObjectHash> {
        match self {
            Self::File { hash, .. } | Self::Symlink { hash, .. } => Some(*hash),
            Self::Directory { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
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
            Some(EntryKind::Directory) => {}
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
        FsTreeEntry::File { mode, .. } | FsTreeEntry::Directory { mode, .. } => *mode,
        FsTreeEntry::Symlink { .. } => return Ok(()),
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
            require_exact_keys(object, &["p", "t", "u", "g", "m", "h"], line_number)?;
            Ok(FsTreeEntry::File {
                path,
                uid: required_u32(object, "u", line_number)?,
                gid: required_u32(object, "g", line_number)?,
                mode: required_mode(object, "m", line_number)?,
                hash: required_hash(object, "h", line_number)?,
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
            require_exact_keys(object, &["p", "t", "u", "g", "x", "h"], line_number)?;
            Ok(FsTreeEntry::Symlink {
                path,
                uid: required_u32(object, "u", line_number)?,
                gid: required_u32(object, "g", line_number)?,
                target: required_string(object, "x", line_number)?.to_string(),
                hash: required_hash(object, "h", line_number)?,
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

fn required_hash(
    object: &Map<String, Value>,
    key: &str,
    line_number: usize,
) -> Result<ObjectHash, FsTreeManifestError> {
    let raw = required_string(object, key, line_number)?;
    ObjectHash::from_str(raw).map_err(|error| {
        FsTreeManifestError::Parse(format!(
            "key '{key}' on fs-tree manifest line {line_number} must be a lowercase fsobj hash: {error}"
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
        FsTreeEntry::File {
            path,
            uid,
            gid,
            mode,
            hash,
        } => {
            out.extend_from_slice(br#"{"p":""#);
            write_canonical_string(path, out)?;
            out.extend_from_slice(br#","t":"f","u":"#);
            write_u32(*uid, out);
            out.extend_from_slice(br#","g":"#);
            write_u32(*gid, out);
            out.extend_from_slice(br#","m":"#);
            write_u32(*mode, out);
            out.extend_from_slice(br#","h":""#);
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
            hash,
        } => {
            out.extend_from_slice(br#"{"p":""#);
            write_canonical_string(path, out)?;
            out.extend_from_slice(br#","t":"l","u":"#);
            write_u32(*uid, out);
            out.extend_from_slice(br#","g":"#);
            write_u32(*gid, out);
            out.extend_from_slice(br#","x":""#);
            write_canonical_string(target, out)?;
            out.extend_from_slice(br#","h":""#);
            out.extend_from_slice(hash.to_string().as_bytes());
            out.push(b'"');
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

fn printable_path(path: &str) -> String {
    path.chars().flat_map(char::escape_default).collect()
}

fn placeholder_leaf_hash() -> ObjectHash {
    ObjectHash::from_str("0000000000000000000000000000000000000000000000000000000000000000")
        .expect("placeholder hash is valid lowercase hex")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_parse_rejects(&format!("{line}\n"));
    }

    fn assert_entries_reject(entries: Vec<FsTreeEntry>) {
        assert!(
            FsTreeManifest::from_entries(entries).is_err(),
            "entries were accepted"
        );
    }

    #[test]
    fn writer_emits_exact_canonical_jsonl() {
        let manifest = manifest(vec![
            root(),
            FsTreeEntry::directory("bin", 0, 0, 0o755),
            FsTreeEntry::file("bin/tool", 1000, 1001, 0o755),
            FsTreeEntry::symlink("tool-link", 2, 3, "bin/tool"),
        ]);

        assert_eq!(
            String::from_utf8(manifest.to_canonical_bytes().unwrap()).unwrap(),
            concat!(
                r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
                "\n",
                r#"{"p":"bin","t":"d","u":0,"g":0,"m":493}"#,
                "\n",
                r#"{"p":"bin/tool","t":"f","u":1000,"g":1001,"m":493,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
                "\n",
                r#"{"p":"tool-link","t":"l","u":2,"g":3,"x":"bin/tool","h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
                "\n",
            )
        );
    }

    #[test]
    fn from_entries_sorts_by_path_bytes() {
        let manifest = manifest(vec![
            FsTreeEntry::file("b", 0, 0, 0o644),
            FsTreeEntry::file("a", 0, 0, 0o644),
            root(),
        ]);

        let paths: Vec<&str> = manifest.entries().iter().map(FsTreeEntry::path).collect();
        assert_eq!(paths, vec!["", "a", "b"]);
    }

    #[test]
    fn parser_accepts_canonical_sample_and_round_trips() {
        let bytes = concat!(
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"a","t":"f","u":1,"g":2,"m":420,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
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
        let manifest = manifest(vec![root(), FsTreeEntry::file("a", 1, 2, 0o644)]);

        manifest.write_canonical(&path).unwrap();
        let read = FsTreeManifest::read_canonical(&path).unwrap();

        assert_eq!(read, manifest);
    }

    #[test]
    fn rejects_missing_final_newline_empty_file_and_empty_line() {
        assert_parse_rejects(r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#);
        assert!(FsTreeManifest::parse_canonical_bytes(b"").is_err());
        assert_parse_rejects(concat!(r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#, "\n\n"));
    }

    #[test]
    fn rejects_wrong_key_order_unknown_missing_and_duplicate_fields() {
        assert_line_rejects(r#"{"t":"d","p":"","u":0,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0,"m":493,"x":1}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0,"m":493,"u":0}"#);
    }

    #[test]
    fn rejects_symlink_mode_and_missing_symlink_target_field() {
        assert_parse_rejects(concat!(
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"l","t":"l","u":0,"g":0,"m":511}"#,
            "\n",
        ));
        assert_parse_rejects(concat!(
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"l","t":"l","u":0,"g":0}"#,
            "\n",
        ));
    }

    #[test]
    fn rejects_duplicate_paths() {
        assert_entries_reject(vec![
            root(),
            FsTreeEntry::file("a", 0, 0, 0o644),
            FsTreeEntry::directory("a", 0, 0, 0o755),
        ]);
        assert_parse_rejects(concat!(
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"a","t":"f","u":0,"g":0,"m":420,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            "\n",
            r#"{"p":"a","t":"f","u":0,"g":0,"m":420,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            "\n",
        ));
    }

    #[test]
    fn parser_rejects_entries_outside_canonical_order() {
        assert_parse_rejects(concat!(
            r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
            "\n",
            r#"{"p":"b","t":"f","u":0,"g":0,"m":420,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            "\n",
            r#"{"p":"a","t":"f","u":0,"g":0,"m":420,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            "\n",
        ));
    }

    #[test]
    fn rejects_missing_root_non_directory_root_missing_parent_and_non_directory_parent() {
        assert_entries_reject(vec![FsTreeEntry::file("a", 0, 0, 0o644)]);
        assert_entries_reject(vec![FsTreeEntry::file("", 0, 0, 0o644)]);
        assert_entries_reject(vec![root(), FsTreeEntry::file("a/b", 0, 0, 0o644)]);
        assert_entries_reject(vec![
            root(),
            FsTreeEntry::file("a", 0, 0, 0o644),
            FsTreeEntry::file("a/b", 0, 0, 0o644),
        ]);
    }

    #[test]
    fn rejects_malformed_paths() {
        for path in ["/a", ".", "..", "a//b", "a/", "a/.", "a/..", "a\nb"] {
            assert_entries_reject(vec![root(), FsTreeEntry::file(path, 0, 0, 0o644)]);
        }
    }

    #[test]
    fn rejects_invalid_number_ranges_and_shapes() {
        assert_entries_reject(vec![root(), FsTreeEntry::file("a", 0, 0, 0o10000)]);
        assert_line_rejects(r#"{"p":"","t":"d","u":4294967296,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":4294967296,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":0,"g":0,"m":4096}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":-1,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":1.0,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":null,"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":[],"g":0,"m":493}"#);
        assert_line_rejects(r#"{"p":"","t":"d","u":{},"g":0,"m":493}"#);
    }

    #[test]
    fn writer_and_parser_handle_quote_and_backslash_paths() {
        let manifest = manifest(vec![
            root(),
            FsTreeEntry::file(r#"a"b"#, 0, 0, 0o644),
            FsTreeEntry::file(r#"a\b"#, 0, 0, 0o644),
        ]);
        let bytes = manifest.to_canonical_bytes().unwrap();

        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            concat!(
                r#"{"p":"","t":"d","u":0,"g":0,"m":493}"#,
                "\n",
                r#"{"p":"a\"b","t":"f","u":0,"g":0,"m":420,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
                "\n",
                r#"{"p":"a\\b","t":"f","u":0,"g":0,"m":420,"h":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
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
        assert_line_rejects(r#"{"p":"a\/b","t":"f","u":0,"g":0,"m":420}"#);
        assert_line_rejects(r#"{"p":"\u0061","t":"f","u":0,"g":0,"m":420}"#);
        assert_line_rejects(r#"{ "p":"","t":"d","u":0,"g":0,"m":493}"#);
    }
}
