use crate::fsutil;
use fsobj_hash::{ObjectHash, hash_path};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::env;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const ARTIFACT_SCHEMA: &str = "mbuild-artifact-v1";
const ROOT_DIR: &str = ".mbuild";
const OBJECTS_DIR: &str = "objects";
const ARTIFACTS_DIR: &str = "artifacts";
const OBJECT_REFS_DIR: &str = "object-refs";
const ARTIFACT_REFS_DIR: &str = "artifact-refs";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactHash([u8; 32]);

impl ArtifactHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    pub fn to_prefixed_hex(&self) -> String {
        format!("sha256:{}", self.to_hex())
    }
}

impl fmt::Display for ArtifactHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256:")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ArtifactHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ArtifactHash")
            .field(&self.to_prefixed_hex())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseArtifactHashError {
    MissingPrefix,
    InvalidLength,
    InvalidHex,
}

impl fmt::Display for ParseArtifactHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => f.write_str("missing sha256: prefix"),
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

impl std::error::Error for ParseArtifactHashError {}

impl FromStr for ArtifactHash {
    type Err = ParseArtifactHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or(ParseArtifactHashError::MissingPrefix)?;
        if hex.len() != 64 {
            return Err(ParseArtifactHashError::InvalidLength);
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ParseArtifactHashError::InvalidHex);
        }

        let mut bytes = [0u8; 32];
        for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = decode_nibble(chunk[0]).ok_or(ParseArtifactHashError::InvalidHex)?;
            let lo = decode_nibble(chunk[1]).ok_or(ParseArtifactHashError::InvalidHex)?;
            bytes[idx] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Debug)]
pub enum CasError {
    Io(String),
    InvalidInput(String),
    Hashing(String),
    Serialization(String),
}

impl fmt::Display for CasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message)
            | Self::InvalidInput(message)
            | Self::Hashing(message)
            | Self::Serialization(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CasError {}

#[derive(Debug, Clone)]
pub struct StoreLayout {
    pub root: PathBuf,
    pub objects: PathBuf,
    pub artifacts: PathBuf,
    pub object_refs: PathBuf,
    pub artifact_refs: PathBuf,
}

impl StoreLayout {
    pub fn discover(root: &Path) -> Result<Self, CasError> {
        let layout = Self {
            root: root.to_path_buf(),
            objects: root.join(OBJECTS_DIR),
            artifacts: root.join(ARTIFACTS_DIR),
            object_refs: root.join(OBJECT_REFS_DIR),
            artifact_refs: root.join(ARTIFACT_REFS_DIR),
        };
        layout.ensure()?;
        Ok(layout)
    }

    pub fn discover_in_cwd() -> Result<Self, CasError> {
        let cwd = env::current_dir()
            .map_err(|error| CasError::Io(format!("failed to get current directory: {error}")))?;
        Self::discover(&cwd.join(ROOT_DIR))
    }

    fn ensure(&self) -> Result<(), CasError> {
        ensure_dir(&self.root, "mbuild root")?;
        ensure_dir(&self.objects, "objects")?;
        ensure_dir(&self.artifacts, "artifacts")?;
        ensure_dir(&self.object_refs, "object-refs")?;
        ensure_dir(&self.artifact_refs, "artifact-refs")?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct PublishOutputRequest {
    pub output_name: String,
    pub staged_path: PathBuf,
    pub artifact_kind: String,
    pub producer_builder: String,
    pub input_artifact_hashes: Vec<ArtifactHash>,
    pub attrs: Map<String, Value>,
}

#[derive(Debug, Clone, Copy)]
pub struct PublishedOutput {
    pub object_hash: ObjectHash,
    pub artifact_hash: ArtifactHash,
}

pub fn publish_output(
    layout: &StoreLayout,
    request: PublishOutputRequest,
) -> Result<PublishedOutput, CasError> {
    validate_output_name(&request.output_name)?;
    let object_hash = import_object(layout, &request.staged_path)?;

    let artifact_value = artifact_json_value(
        &request.artifact_kind,
        object_hash,
        &request.producer_builder,
        &request.input_artifact_hashes,
        request.attrs,
    );
    let canonical = canonical_json_bytes(&artifact_value)?;
    let artifact_hash = ArtifactHash(Sha256::digest(&canonical).into());
    let artifact_path = layout
        .artifacts
        .join(format!("{}.json", artifact_hash.to_prefixed_hex()));
    fsutil::write_atomic(
        &artifact_path,
        std::str::from_utf8(&canonical).map_err(|error| {
            CasError::Serialization(format!(
                "failed to encode canonical artifact JSON as UTF-8: {error}"
            ))
        })?,
    )
    .map_err(map_fsutil_error)?;

    let object_ref_path = layout.object_refs.join(&request.output_name);
    let object_ref_target = PathBuf::from("..")
        .join(OBJECTS_DIR)
        .join(object_hash.to_prefixed_hex());
    replace_symlink(&object_ref_target, &object_ref_path)?;

    let artifact_ref_path = layout
        .artifact_refs
        .join(format!("{}.json", request.output_name));
    let artifact_ref_target = PathBuf::from("..")
        .join(ARTIFACTS_DIR)
        .join(format!("{}.json", artifact_hash.to_prefixed_hex()));
    replace_symlink(&artifact_ref_target, &artifact_ref_path)?;

    Ok(PublishedOutput {
        object_hash,
        artifact_hash,
    })
}

fn import_object(layout: &StoreLayout, staged_path: &Path) -> Result<ObjectHash, CasError> {
    let object_hash = hash_path(staged_path).map_err(|error| {
        CasError::Hashing(format!(
            "failed to hash staged object '{}': {error}",
            staged_path.display()
        ))
    })?;
    let destination = layout.objects.join(object_hash.to_prefixed_hex());
    if destination.exists() {
        remove_path_force(staged_path)?;
        return Ok(object_hash);
    }

    fs::rename(staged_path, &destination).map_err(|error| {
        CasError::Io(format!(
            "failed to import object '{}' -> '{}': {error}",
            staged_path.display(),
            destination.display()
        ))
    })?;

    Ok(object_hash)
}

fn artifact_json_value(
    artifact_kind: &str,
    object_hash: ObjectHash,
    producer_builder: &str,
    input_artifact_hashes: &[ArtifactHash],
    attrs: Map<String, Value>,
) -> Value {
    let mut producer = Map::new();
    producer.insert(
        "builder".to_string(),
        Value::String(producer_builder.to_string()),
    );

    let input_hashes = input_artifact_hashes
        .iter()
        .map(|hash| Value::String(hash.to_prefixed_hex()))
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(ARTIFACT_SCHEMA.to_string()),
    );
    root.insert(
        "artifact_kind".to_string(),
        Value::String(artifact_kind.to_string()),
    );
    root.insert(
        "object_hash".to_string(),
        Value::String(object_hash.to_prefixed_hex()),
    );
    root.insert("producer".to_string(), Value::Object(producer));
    root.insert(
        "input_artifact_hashes".to_string(),
        Value::Array(input_hashes),
    );
    root.insert("attrs".to_string(), Value::Object(attrs));
    Value::Object(root)
}

fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, CasError> {
    let mut out = Vec::new();
    write_canonical_json(value, &mut out)?;
    Ok(out)
}

fn write_canonical_json(value: &Value, out: &mut Vec<u8>) -> Result<(), CasError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_writer(out, value).map_err(|error| {
                CasError::Serialization(format!("failed to serialize JSON value: {error}"))
            })
        }
        Value::Array(items) => {
            out.push(b'[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                write_canonical_json(item, out)?;
            }
            out.push(b']');
            Ok(())
        }
        Value::Object(object) => {
            out.push(b'{');
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            for (idx, key) in keys.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).map_err(|error| {
                    CasError::Serialization(format!("failed to serialize JSON key: {error}"))
                })?;
                out.push(b':');
                write_canonical_json(&object[*key], out)?;
            }
            out.push(b'}');
            Ok(())
        }
    }
}

fn ensure_dir(path: &Path, label: &str) -> Result<(), CasError> {
    fs::create_dir_all(path).map_err(|error| {
        CasError::Io(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

fn validate_output_name(name: &str) -> Result<(), CasError> {
    if name.is_empty() {
        return Err(CasError::InvalidInput(
            "output name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(CasError::InvalidInput(format!(
            "invalid output name '{name}'"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(CasError::InvalidInput(format!(
            "invalid output name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }
    Ok(())
}

fn remove_path_force(path: &Path) -> Result<(), CasError> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CasError::Io(format!(
            "failed to inspect path '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_dir() {
        fsutil::remove_dir_force(path).map_err(map_fsutil_error)
    } else {
        fs::remove_file(path).map_err(|error| {
            CasError::Io(format!(
                "failed to remove file '{}': {error}",
                path.display()
            ))
        })
    }
}

fn replace_symlink(target: &Path, link_path: &Path) -> Result<(), CasError> {
    if link_path.exists() || link_path.is_symlink() {
        let metadata = fs::symlink_metadata(link_path).map_err(|error| {
            CasError::Io(format!(
                "failed to inspect existing ref '{}': {error}",
                link_path.display()
            ))
        })?;
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(link_path).map_err(|error| {
                CasError::Io(format!(
                    "failed to remove existing ref directory '{}': {error}",
                    link_path.display()
                ))
            })?;
        } else {
            fs::remove_file(link_path).map_err(|error| {
                CasError::Io(format!(
                    "failed to remove existing ref '{}': {error}",
                    link_path.display()
                ))
            })?;
        }
    }
    create_symlink(target, link_path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> Result<(), CasError> {
    unix_fs::symlink(target, link_path).map_err(|error| {
        CasError::Io(format!(
            "failed to create ref symlink '{}' -> '{}': {error}",
            link_path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link_path: &Path) -> Result<(), CasError> {
    Err(CasError::Io(
        "symlink refs are currently supported only on unix hosts".to_string(),
    ))
}

fn map_fsutil_error(error: fsutil::FsUtilError) -> CasError {
    CasError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn canonical_json_hash_is_stable_across_key_order() {
        let mut attrs_a = Map::new();
        attrs_a.insert("z".to_string(), Value::from(1));
        attrs_a.insert("a".to_string(), Value::from(true));
        let left = artifact_json_value(
            "text",
            parse_object_hash(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            ),
            "text",
            &[],
            attrs_a,
        );

        let mut attrs_b = Map::new();
        attrs_b.insert("a".to_string(), Value::from(true));
        attrs_b.insert("z".to_string(), Value::from(1));
        let right = artifact_json_value(
            "text",
            parse_object_hash(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            ),
            "text",
            &[],
            attrs_b,
        );

        assert_eq!(
            canonical_json_bytes(&left).unwrap(),
            canonical_json_bytes(&right).unwrap()
        );
    }

    #[test]
    fn publish_output_reuses_existing_object_hash() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "hello".to_string(),
                staged_path: first_stage,
                artifact_kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_artifact_hashes: vec![],
                attrs: Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&second_stage, fs::Permissions::from_mode(0o755)).unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "hello-exec".to_string(),
                staged_path: second_stage,
                artifact_kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_artifact_hashes: vec![],
                attrs: Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
            },
        )
        .unwrap();

        assert_ne!(first.object_hash, second.object_hash);
        assert!(
            layout
                .objects
                .join(first.object_hash.to_prefixed_hex())
                .exists()
        );
        assert!(
            layout
                .artifacts
                .join(format!("{}.json", second.artifact_hash.to_prefixed_hex()))
                .exists()
        );
    }

    fn parse_object_hash(value: &str) -> ObjectHash {
        ObjectHash::from_str(value).unwrap()
    }
}
