use crate::builder::{Build, ProducerInfo, PublishedBuild, StagedBuildResult};
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
const INVOCATION_SCHEMA: &str = "mbuild-build-invocation-v1";
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
    pub build_key: BuildKey,
    pub staged_path: PathBuf,
    pub kind: String,
    pub producer_builder: String,
    pub input_build_keys: Vec<BuildKey>,
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
    let staged = StagedBuildResult {
        kind: request.kind,
        producer: ProducerInfo {
            builder: request.producer_builder,
        },
        input_build_keys: request.input_build_keys,
        attrs: request.attrs,
        staged_path: request.staged_path,
    };
    let published = materialize_build(layout, request.build_key, staged)?;
    publish_refs(layout, &request.output_name, &published)?;

    Ok(PublishedOutput {
        object_hash: published.record.object_hash,
        build_key: published.record.build_key,
    })
}

pub fn compute_build_key(
    builder_tag: &str,
    normalized_payload: &Value,
    input_build_keys: &[BuildKey],
) -> Result<BuildKey, CasError> {
    let input_keys = input_build_keys
        .iter()
        .map(|key| Value::String(key.to_string()))
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(INVOCATION_SCHEMA.to_string()),
    );
    root.insert(
        "builder_tag".to_string(),
        Value::String(builder_tag.to_string()),
    );
    root.insert("payload".to_string(), normalized_payload.clone());
    root.insert(
        "input_build_keys".to_string(),
        Value::Array(input_keys),
    );

    let canonical = canonical_json_bytes(&Value::Object(root))?;
    Ok(BuildKey(Sha256::digest(&canonical).into()))
}

pub fn load_build_record(
    layout: &StoreLayout,
    build_key: BuildKey,
) -> Result<Option<Build>, CasError> {
    let build_path = layout
        .builds
        .join(format!("{}.json", build_key.to_prefixed_hex()));
    if !build_path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&build_path).map_err(|error| {
        CasError::Io(format!(
            "failed to read build record '{}': {error}",
            build_path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        CasError::Serialization(format!(
            "failed to parse build record '{}': {error}",
            build_path.display()
        ))
    })?;
    Ok(Some(parse_build_record_value(build_key, &value)?))
}

pub fn materialize_build(
    layout: &StoreLayout,
    build_key: BuildKey,
    staged: StagedBuildResult,
) -> Result<PublishedBuild, CasError> {
    let object_hash = import_object(layout, &staged.staged_path)?;
    let record = Build {
        build_key,
        object_hash,
        kind: staged.kind,
        producer: staged.producer,
        input_build_keys: staged.input_build_keys,
        attrs: staged.attrs,
    };
    write_build_record(layout, &record)?;

    Ok(PublishedBuild {
        object_path: layout.objects.join(object_hash.to_prefixed_hex()),
        record,
    })
}

pub fn publish_refs(
    layout: &StoreLayout,
    output_name: &str,
    published: &PublishedBuild,
) -> Result<(), CasError> {
    validate_output_name(output_name)?;

    let object_ref_path = layout.object_refs.join(output_name);
    let object_ref_target = PathBuf::from("..")
        .join(OBJECTS_DIR)
        .join(published.record.object_hash.to_prefixed_hex());
    replace_symlink(&object_ref_target, &object_ref_path)?;

    let meta_ref_path = layout.meta_refs.join(format!("{output_name}.json"));
    let meta_ref_target = PathBuf::from("..")
        .join(BUILDS_DIR)
        .join(format!("{}.json", published.record.build_key.to_prefixed_hex()));
    replace_symlink(&meta_ref_target, &meta_ref_path)?;
    Ok(())
}

pub fn object_path(layout: &StoreLayout, object_hash: ObjectHash) -> PathBuf {
    layout.objects.join(object_hash.to_prefixed_hex())
}

pub fn build_path(layout: &StoreLayout, build_key: BuildKey) -> PathBuf {
    layout.builds.join(format!("{}.json", build_key.to_prefixed_hex()))
}

pub fn import_object(layout: &StoreLayout, staged_path: &Path) -> Result<ObjectHash, CasError> {
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

fn write_build_record(layout: &StoreLayout, record: &Build) -> Result<(), CasError> {
    let build_value = build_record_json_value(record);
    let canonical = canonical_json_bytes(&build_value)?;
    let build_path = build_path(layout, record.build_key);
    fsutil::write_atomic(
        &build_path,
        std::str::from_utf8(&canonical).map_err(|error| {
            CasError::Serialization(format!(
                "failed to encode canonical build JSON as UTF-8: {error}"
            ))
        })?,
    )
    .map_err(map_fsutil_error)
}

fn build_json_value(
    build_key: BuildKey,
    kind: &str,
    object_hash: ObjectHash,
    producer_builder: &str,
    input_build_keys: &[BuildKey],
    attrs: Map<String, Value>,
) -> Value {
    let mut producer = Map::new();
    producer.insert(
        "builder".to_string(),
        Value::String(producer_builder.to_string()),
    );

    let input_keys = input_build_keys
        .iter()
        .map(|key| Value::String(key.to_string()))
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(BUILD_SCHEMA.to_string()),
    );
    root.insert(
        "build_key".to_string(),
        Value::String(build_key.to_string()),
    );
    root.insert("kind".to_string(), Value::String(kind.to_string()));
    root.insert(
        "object_hash".to_string(),
        Value::String(object_hash.to_string()),
    );
    root.insert("producer".to_string(), Value::Object(producer));
    root.insert(
        "input_build_keys".to_string(),
        Value::Array(input_keys),
    );
    root.insert("attrs".to_string(), Value::Object(attrs));
    Value::Object(root)
}

fn build_record_json_value(record: &Build) -> Value {
    build_json_value(
        record.build_key,
        &record.kind,
        record.object_hash,
        &record.producer.builder,
        &record.input_build_keys,
        record.attrs.clone(),
    )
}

fn parse_build_record_value(build_key: BuildKey, value: &Value) -> Result<Build, CasError> {
    let object = value.as_object().ok_or_else(|| {
        CasError::Serialization("build record root must be a JSON object".to_string())
    })?;

    let schema = object
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("build record is missing 'schema'".to_string()))?;
    if schema != BUILD_SCHEMA {
        return Err(CasError::Serialization(format!(
            "unsupported build record schema '{schema}'"
        )));
    }

    let encoded_build_key = object
        .get("build_key")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("build record is missing 'build_key'".to_string()))
        .and_then(parse_build_key_result)?;
    if encoded_build_key != build_key {
        return Err(CasError::Serialization(format!(
            "build record key mismatch: path key '{}' does not match encoded key '{}'",
            build_key, encoded_build_key
        )));
    }

    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("build record is missing 'kind'".to_string()))?
        .to_string();

    let object_hash = object
        .get("object_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("build record is missing 'object_hash'".to_string()))
        .and_then(parse_object_hash_result)?;

    let producer_obj = object
        .get("producer")
        .and_then(Value::as_object)
        .ok_or_else(|| CasError::Serialization("build record is missing 'producer'".to_string()))?;
    let builder = producer_obj
        .get("builder")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CasError::Serialization("build record producer is missing 'builder'".to_string())
        })?
        .to_string();

    let input_build_keys = object
        .get("input_build_keys")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CasError::Serialization("build record is missing 'input_build_keys'".to_string())
        })?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| {
                    CasError::Serialization(
                        "build record input_build_keys must contain strings".to_string(),
                    )
                })
                .and_then(parse_build_key_result)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let attrs = object
        .get("attrs")
        .and_then(Value::as_object)
        .ok_or_else(|| CasError::Serialization("build record is missing 'attrs'".to_string()))?
        .clone();

    Ok(Build {
        build_key,
        object_hash,
        kind,
        producer: ProducerInfo { builder },
        input_build_keys,
        attrs,
    })
}

fn parse_object_hash_result(value: &str) -> Result<ObjectHash, CasError> {
    value.parse::<ObjectHash>().map_err(|error| {
        CasError::Serialization(format!("invalid object hash '{value}' in build record: {error}"))
    })
}

fn parse_build_key_result(value: &str) -> Result<BuildKey, CasError> {
    value.parse::<BuildKey>().map_err(|error| {
        CasError::Serialization(format!("invalid build key '{value}' in build record: {error}"))
    })
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
    use serde_json::json;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn canonical_json_hash_is_stable_across_key_order() {
        let build_key = parse_build_key(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let mut attrs_a = Map::new();
        attrs_a.insert("z".to_string(), Value::from(1));
        attrs_a.insert("a".to_string(), Value::from(true));
        let left = build_json_value(
            build_key,
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
            build_key,
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
                build_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "echo hi\n" }),
                    &[parse_build_key(
                        "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                    )],
                ),
                staged_path: stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![parse_build_key(
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
            build_json["build_key"],
            Value::String(published.build_key.to_string())
        );
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
            build_json["input_build_keys"],
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
    fn same_object_different_payload_produces_different_build_key() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "first".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello" }),
                    &[],
                ),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "source-tree", "source": "hello" }),
                    &[],
                ),
                staged_path: second_stage,
                kind: "source-tree".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for("Text", json!({ "kind": "source-tree" }), &[]),
                staged_path: second_stage,
                kind: "source-tree".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
    }

    #[test]
    fn build_key_changes_when_builder_tag_changes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "producer-a".to_string(),
                build_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for("Fetch", json!({ "kind": "build-script" }), &[]),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "fetch".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello" }),
                    &[],
                ),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello world" }),
                    &[],
                ),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                    build_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                    staged_path: stage,
                    kind: "build-script".to_string(),
                    producer_builder: "text".to_string(),
                    input_build_keys: vec![],
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
                build_key: build_key_for("Fetch", json!({ "kind": "source-tree" }), &[]),
                staged_path: stage_dir,
                kind: "source-tree".to_string(),
                producer_builder: "fetch".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                staged_path: second_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
                attrs: Map::new(),
            },
        )
        .unwrap();

        assert!(!second_stage_path.exists());
    }

    #[test]
    fn build_key_changes_when_input_build_key_order_changes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let key_a = parse_build_key(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let key_b = parse_build_key(
            "sha256:2222222222222222222222222222222222222222222222222222222222222222",
        );

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "ordered-ab".to_string(),
                build_key: build_key_for(
                    "Binary",
                    json!({ "kind": "binary-output" }),
                    &[key_a, key_b],
                ),
                staged_path: first_stage,
                kind: "binary-output".to_string(),
                producer_builder: "binary".to_string(),
                input_build_keys: vec![key_a, key_b],
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
                build_key: build_key_for(
                    "Binary",
                    json!({ "kind": "binary-output" }),
                    &[key_b, key_a],
                ),
                staged_path: second_stage,
                kind: "binary-output".to_string(),
                producer_builder: "binary".to_string(),
                input_build_keys: vec![key_b, key_a],
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
    fn executable_bit_changes_object_hash_for_distinct_invocations() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("plain.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "plain".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello", "variant": "plain" }),
                    &[],
                ),
                staged_path: first_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello", "variant": "exec" }),
                    &[],
                ),
                staged_path: exec_stage,
                kind: "build-script".to_string(),
                producer_builder: "text".to_string(),
                input_build_keys: vec![],
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

    fn parse_build_key(value: &str) -> BuildKey {
        BuildKey::from_str(value).unwrap()
    }

    fn build_key_for(
        builder_tag: &str,
        payload: Value,
        input_build_keys: &[BuildKey],
    ) -> BuildKey {
        compute_build_key(builder_tag, &payload, input_build_keys).unwrap()
    }
}
