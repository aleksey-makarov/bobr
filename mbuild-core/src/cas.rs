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

const BUILD_SCHEMA: &str = "mbuild-build-v1";
const ROOT_DIR: &str = ".mbuild";
const OBJECTS_DIR: &str = "objects";
const BUILDS_DIR: &str = "builds";
const OBJECT_REFS_DIR: &str = "object-refs";
const META_REFS_DIR: &str = "meta-refs";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuildKey([u8; 32]);

impl BuildKey {
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

impl fmt::Display for BuildKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256:")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BuildKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BuildKey")
            .field(&self.to_prefixed_hex())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseBuildKeyError {
    MissingPrefix,
    InvalidLength,
    InvalidHex,
}

impl fmt::Display for ParseBuildKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => f.write_str("missing sha256: prefix"),
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

impl std::error::Error for ParseBuildKeyError {}

impl FromStr for BuildKey {
    type Err = ParseBuildKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or(ParseBuildKeyError::MissingPrefix)?;
        if hex.len() != 64 {
            return Err(ParseBuildKeyError::InvalidLength);
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ParseBuildKeyError::InvalidHex);
        }

        let mut bytes = [0u8; 32];
        for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = decode_nibble(chunk[0]).ok_or(ParseBuildKeyError::InvalidHex)?;
            let lo = decode_nibble(chunk[1]).ok_or(ParseBuildKeyError::InvalidHex)?;
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
    pub builds: PathBuf,
    pub object_refs: PathBuf,
    pub meta_refs: PathBuf,
}

impl StoreLayout {
    pub fn discover(root: &Path) -> Result<Self, CasError> {
        let layout = Self {
            root: root.to_path_buf(),
            objects: root.join(OBJECTS_DIR),
            builds: root.join(BUILDS_DIR),
            object_refs: root.join(OBJECT_REFS_DIR),
            meta_refs: root.join(META_REFS_DIR),
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
        ensure_dir(&self.builds, "builds")?;
        ensure_dir(&self.object_refs, "object-refs")?;
        ensure_dir(&self.meta_refs, "meta-refs")?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct PublishOutputRequest {
    pub output_name: String,
    pub staged_path: PathBuf,
    pub kind: String,
    pub producer_builder: String,
    pub input_object_hashes: Vec<ObjectHash>,
    pub attrs: Map<String, Value>,
}

#[derive(Debug, Clone, Copy)]
pub struct PublishedOutput {
    pub object_hash: ObjectHash,
    pub build_key: BuildKey,
}

pub fn publish_output(
    layout: &StoreLayout,
    request: PublishOutputRequest,
) -> Result<PublishedOutput, CasError> {
    validate_output_name(&request.output_name)?;
    let object_hash = import_object(layout, &request.staged_path)?;

    let build_value = build_json_value(
        &request.kind,
        object_hash,
        &request.producer_builder,
        &request.input_object_hashes,
        request.attrs,
    );
    let canonical = canonical_json_bytes(&build_value)?;
    let build_key = BuildKey(Sha256::digest(&canonical).into());
    let build_path = layout
        .builds
        .join(format!("{}.json", build_key.to_prefixed_hex()));
    fsutil::write_atomic(
        &build_path,
        std::str::from_utf8(&canonical).map_err(|error| {
            CasError::Serialization(format!(
                "failed to encode canonical build JSON as UTF-8: {error}"
            ))
        })?,
    )
    .map_err(map_fsutil_error)?;

    let object_ref_path = layout.object_refs.join(&request.output_name);
    let object_ref_target = PathBuf::from("..").join(OBJECTS_DIR).join(object_hash.to_prefixed_hex());
    replace_symlink(&object_ref_target, &object_ref_path)?;

    let meta_ref_path = layout.meta_refs.join(format!("{}.json", request.output_name));
    let meta_ref_target = PathBuf::from("..").join(BUILDS_DIR).join(format!("{}.json", build_key.to_prefixed_hex()));
    replace_symlink(&meta_ref_target, &meta_ref_path)?;

    Ok(PublishedOutput {
        object_hash,
        build_key,
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

fn build_json_value(
    kind: &str,
    object_hash: ObjectHash,
    producer_builder: &str,
    input_object_hashes: &[ObjectHash],
    attrs: Map<String, Value>,
) -> Value {
    let mut producer = Map::new();
    producer.insert(
        "builder".to_string(),
        Value::String(producer_builder.to_string()),
    );

    let input_hashes = input_object_hashes
        .iter()
        .map(|hash| Value::String(hash.to_string()))
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(BUILD_SCHEMA.to_string()),
    );
    root.insert("kind".to_string(), Value::String(kind.to_string()));
    root.insert(
        "object_hash".to_string(),
        Value::String(object_hash.to_string()),
    );
    root.insert("producer".to_string(), Value::Object(producer));
    root.insert(
        "input_object_hashes".to_string(),
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
        let left = build_json_value(
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
        let right = build_json_value(
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
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "hello-copy".to_string(),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_eq!(first.build_key, second.build_key);
        assert!(layout.objects.join(first.object_hash.to_prefixed_hex()).exists());
        assert!(
            layout
                .builds
                .join(format!("{}.json", second.build_key.to_prefixed_hex()))
                .exists()
        );
        assert_eq!(
            fs::read_link(layout.meta_refs.join("hello-copy.json")).unwrap(),
            PathBuf::from("..").join(BUILDS_DIR).join(format!(
                "{}.json",
                second.build_key.to_prefixed_hex()
            ))
        );
    }

    #[test]
    fn publish_output_writes_build_record_and_refs() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let stage = temp.path().join("script.sh");
        fs::write(&stage, b"echo hi\n").unwrap();
        let published = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "script".to_string(),
                staged_path: stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![parse_object_hash(
                    "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                )],
                attrs: Map::from_iter([
                    ("source_bytes".to_string(), Value::from(8)),
                    ("generated".to_string(), Value::from(false)),
                ]),
            },
        )
        .unwrap();

        let build_path = layout
            .builds
            .join(format!("{}.json", published.build_key.to_prefixed_hex()));
        assert!(build_path.exists());

        let build_json: Value = serde_json::from_slice(&fs::read(&build_path).unwrap()).unwrap();
        assert_eq!(build_json["schema"], Value::String(BUILD_SCHEMA.to_string()));
        assert_eq!(
            build_json["object_hash"],
            Value::String(published.object_hash.to_string())
        );
        assert_eq!(build_json["kind"], Value::String("build-script".to_string()));
        assert_eq!(
            build_json["producer"]["builder"],
            Value::String("text".to_string())
        );
        assert_eq!(
            build_json["input_object_hashes"],
            Value::Array(vec![Value::String(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
            )])
        );
        assert_eq!(build_json["attrs"]["source_bytes"], Value::from(8));
        assert_eq!(build_json["attrs"]["generated"], Value::from(false));

        assert_eq!(
            fs::read_link(layout.meta_refs.join("script.json")).unwrap(),
            PathBuf::from("..")
                .join(BUILDS_DIR)
                .join(format!("{}.json", published.build_key.to_prefixed_hex()))
        );
        assert_eq!(
            fs::read_link(layout.object_refs.join("script")).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(published.object_hash.to_prefixed_hex())
        );
    }

    #[test]
    fn same_object_different_metadata_produces_different_build_key() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "first".to_string(),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "second".to_string(),
                staged_path: second_stage,
                kind: "source-tree".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::from_iter([(String::from("source_bytes"), Value::from(6))]),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
    }

    #[test]
    fn build_key_changes_when_kind_changes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "kind-a".to_string(),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "kind-b".to_string(),
                staged_path: second_stage,
                kind: "source-tree".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
    }

    #[test]
    fn build_key_changes_when_producer_builder_changes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "producer-a".to_string(),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "producer-b".to_string(),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "fetch".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
    }

    #[test]
    fn publish_output_replaces_existing_refs() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "shared".to_string(),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello world").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "shared".to_string(),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert_ne!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
        assert_eq!(
            fs::read_link(layout.object_refs.join("shared")).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(second.object_hash.to_prefixed_hex())
        );
        assert_eq!(
            fs::read_link(layout.meta_refs.join("shared.json")).unwrap(),
            PathBuf::from("..")
                .join(BUILDS_DIR)
                .join(format!("{}.json", second.build_key.to_prefixed_hex()))
        );
    }

    #[test]
    fn invalid_output_name_is_rejected() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        for invalid_name in ["", ".", "..", "bad/name", "bad name"] {
            let stage = temp.path().join(format!(
                "invalid-{}.txt",
                invalid_name.replace(['/', ' '], "_")
            ));
            fs::write(&stage, b"hello").unwrap();

            let error = publish_output(
                &layout,
                PublishOutputRequest {
                    output_name: invalid_name.to_string(),
                    staged_path: stage,
                    kind: "build-script".to_string(),
                    producer_builder: "text".to_string(),
                    input_object_hashes: vec![],
                    attrs: Map::new(),
                },
            )
            .unwrap_err();

            assert!(matches!(error, CasError::InvalidInput(_)));
        }
    }

    #[test]
    fn publish_output_accepts_directory_objects() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let stage_dir = temp.path().join("tree");
        fs::create_dir_all(stage_dir.join("bin")).unwrap();
        fs::write(stage_dir.join("bin").join("tool"), b"echo hi\n").unwrap();

        let published = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "tree".to_string(),
                staged_path: stage_dir,
                kind: "source-tree".to_string(),
                producer_builder: "fetch".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        let object_path = layout.objects.join(published.object_hash.to_prefixed_hex());
        assert!(object_path.is_dir());
        assert!(object_path.join("bin").join("tool").exists());
        assert!(
            layout
                .builds
                .join(format!("{}.json", published.build_key.to_prefixed_hex()))
                .exists()
        );
    }

    #[test]
    fn existing_object_reuse_removes_staged_path() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "first".to_string(),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second_stage_path = second_stage.clone();
        publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "second".to_string(),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert!(!second_stage_path.exists());
    }

    #[test]
    fn build_key_changes_when_input_object_hash_order_changes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let hash_a = parse_object_hash(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let hash_b = parse_object_hash(
            "sha256:2222222222222222222222222222222222222222222222222222222222222222",
        );

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "ordered-ab".to_string(),
                staged_path: first_stage,
                kind: "binary-output".to_string(),
                producer_builder: "binary".to_string(),
                input_object_hashes: vec![hash_a, hash_b],
                attrs: Map::new(),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "ordered-ba".to_string(),
                staged_path: second_stage,
                kind: "binary-output".to_string(),
                producer_builder: "binary".to_string(),
                input_object_hashes: vec![hash_b, hash_a],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
    }

    #[test]
    fn discover_in_cwd_creates_full_layout() {
        let temp = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let layout = StoreLayout::discover_in_cwd().unwrap();

        std::env::set_current_dir(old_cwd).unwrap();

        assert_eq!(layout.root, temp.path().join(ROOT_DIR));
        assert!(layout.objects.is_dir());
        assert!(layout.builds.is_dir());
        assert!(layout.meta_refs.is_dir());
        assert!(layout.object_refs.is_dir());
    }

    #[test]
    fn build_key_display_and_parse_roundtrip() {
        let key = BuildKey::from_str(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();

        assert_eq!(
            key.to_string(),
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            BuildKey::from_str(&key.to_string()).unwrap().as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn executable_bit_changes_object_hash_but_not_store_layout_rules() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("plain.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "plain".to_string(),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        let exec_stage = temp.path().join("exec.txt");
        fs::write(&exec_stage, b"hello").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&exec_stage, fs::Permissions::from_mode(0o755)).unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "exec".to_string(),
                staged_path: exec_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_object_hashes: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert_ne!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
    }

    fn parse_object_hash(value: &str) -> ObjectHash {
        ObjectHash::from_str(value).unwrap()
    }
}
