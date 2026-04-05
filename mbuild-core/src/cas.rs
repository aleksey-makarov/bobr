use crate::builder::{Build, PublishedBuild, ResultInputIdentity, ResultRecord, StagedBuildResult};
use crate::fsutil;
use fsobj_hash::{ObjectHash, hash_path};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::env;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

const RESULT_SCHEMA: &str = "mbuild-result-v3";
#[cfg(test)]
const BUILD_SCHEMA: &str = RESULT_SCHEMA;
const INVOCATION_SCHEMA: &str = "mbuild-build-invocation-v1";
const RESULT_INVOCATION_SCHEMA: &str = "mbuild-build-result-invocation-v2";
const ROOT_DIR: &str = ".mbuild";
const OBJECTS_DIR: &str = "objects";
const BUILDS_DIR: &str = "builds";
const RESULTS_DIR: &str = "results";
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
}

impl fmt::Display for BuildKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BuildKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BuildKey").field(&self.to_hex()).finish()
    }
}

impl Serialize for BuildKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for BuildKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MetaHash([u8; 32]);

impl MetaHash {
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
}

impl fmt::Display for MetaHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for MetaHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MetaHash").field(&self.to_hex()).finish()
    }
}

impl Serialize for MetaHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for MetaHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseBuildKeyError {
    InvalidLength,
    InvalidHex,
}

impl fmt::Display for ParseBuildKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMetaHashError {
    InvalidLength,
    InvalidHex,
}

impl fmt::Display for ParseMetaHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

impl std::error::Error for ParseMetaHashError {}

impl FromStr for MetaHash {
    type Err = ParseMetaHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(ParseMetaHashError::InvalidLength);
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ParseMetaHashError::InvalidHex);
        }

        let mut bytes = [0u8; 32];
        for (idx, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let hi = decode_nibble(chunk[0]).ok_or(ParseMetaHashError::InvalidHex)?;
            let lo = decode_nibble(chunk[1]).ok_or(ParseMetaHashError::InvalidHex)?;
            bytes[idx] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

impl std::error::Error for ParseBuildKeyError {}

impl FromStr for BuildKey {
    type Err = ParseBuildKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(ParseBuildKeyError::InvalidLength);
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ParseBuildKeyError::InvalidHex);
        }

        let mut bytes = [0u8; 32];
        for (idx, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
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
    pub results: PathBuf,
    pub object_refs: PathBuf,
    pub meta_refs: PathBuf,
}

impl StoreLayout {
    pub fn discover(root: &Path) -> Result<Self, CasError> {
        let layout = Self {
            root: root.to_path_buf(),
            objects: root.join(OBJECTS_DIR),
            builds: root.join(BUILDS_DIR),
            results: root.join(RESULTS_DIR),
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
        ensure_dir(&self.results, "results")?;
        ensure_dir(&self.object_refs, "object-refs")?;
        ensure_dir(&self.meta_refs, "meta-refs")?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct PublishOutputRequest {
    pub output_name: String,
    pub build_key: BuildKey,
    pub result_key: BuildKey,
    pub created_at: String,
    pub staged_path: PathBuf,
    pub inputs: Vec<ResultInputIdentity>,
    pub meta: Map<String, Value>,
}

#[derive(Debug, Clone, Copy)]
pub struct PublishedOutput {
    pub object_hash: ObjectHash,
    pub build_key: BuildKey,
    pub result_key: BuildKey,
}

pub fn publish_output(
    layout: &StoreLayout,
    request: PublishOutputRequest,
) -> Result<PublishedOutput, CasError> {
    if let Some(published) = load_build_handle(layout, request.build_key)? {
        remove_path_force(&request.staged_path)?;
        publish_refs(layout, &request.output_name, &published)?;
        return Ok(PublishedOutput {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
            result_key: published.result.result_key,
        });
    }

    if let Some(result) = load_result_record(layout, request.result_key)? {
        let object_path = object_path(layout, result.object_hash);
        if !object_path.exists() {
            return Err(CasError::Io(format!(
                "result '{}' points to missing object '{}'",
                request.result_key,
                object_path.display()
            )));
        }
        remove_path_force(&request.staged_path)?;
        store_build_handle_ref(layout, request.build_key, request.result_key)?;
        let published = PublishedBuild {
            build: build_from_result(request.build_key, &result),
            result,
            object_path,
        };
        publish_refs(layout, &request.output_name, &published)?;
        return Ok(PublishedOutput {
            object_hash: published.build.object_hash,
            build_key: published.build.build_key,
            result_key: published.result.result_key,
        });
    }

    let staged = StagedBuildResult {
        meta: request.meta,
        staged_path: request.staged_path,
    };
    let published = materialize_build(
        layout,
        request.build_key,
        request.result_key,
        &request.created_at,
        request.inputs,
        staged,
    )?;
    publish_refs(layout, &request.output_name, &published)?;

    Ok(PublishedOutput {
        object_hash: published.build.object_hash,
        build_key: published.build.build_key,
        result_key: published.result.result_key,
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
    root.insert("input_build_keys".to_string(), Value::Array(input_keys));

    let canonical = canonical_json_bytes(&Value::Object(root))?;
    Ok(BuildKey(Sha256::digest(&canonical).into()))
}

pub fn compute_result_key(
    builder_tag: &str,
    normalized_payload: &Value,
    inputs: &[ResultInputIdentity],
) -> Result<BuildKey, CasError> {
    let input_values = inputs
        .iter()
        .map(|input| {
            let mut object = Map::new();
            object.insert(
                "object_hash".to_string(),
                Value::String(input.object_hash.to_string()),
            );
            object.insert(
                "meta_hash".to_string(),
                Value::String(input.meta_hash.to_string()),
            );
            Value::Object(object)
        })
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(RESULT_INVOCATION_SCHEMA.to_string()),
    );
    root.insert(
        "builder_tag".to_string(),
        Value::String(builder_tag.to_string()),
    );
    root.insert("payload".to_string(), normalized_payload.clone());
    root.insert("inputs".to_string(), Value::Array(input_values));

    let canonical = canonical_json_bytes(&Value::Object(root))?;
    Ok(BuildKey(Sha256::digest(&canonical).into()))
}

pub fn compute_meta_hash(meta: &Map<String, Value>) -> Result<MetaHash, CasError> {
    let canonical = canonical_json_bytes(&Value::Object(meta.clone()))?;
    Ok(MetaHash(Sha256::digest(&canonical).into()))
}

pub fn require_meta_kind(meta: &Map<String, Value>) -> Result<&str, CasError> {
    meta.get("kind").and_then(Value::as_str).ok_or_else(|| {
        CasError::Serialization("result metadata is missing string 'kind'".to_string())
    })
}

pub fn load_public_build(
    layout: &StoreLayout,
    build_key: BuildKey,
) -> Result<Option<Build>, CasError> {
    Ok(load_build_handle(layout, build_key)?.map(|published| published.build))
}

pub fn materialize_build(
    layout: &StoreLayout,
    build_key: BuildKey,
    result_key: BuildKey,
    created_at: &str,
    inputs: Vec<ResultInputIdentity>,
    staged: StagedBuildResult,
) -> Result<PublishedBuild, CasError> {
    let object_hash = import_object(layout, &staged.staged_path)?;
    let meta_hash = compute_meta_hash(&staged.meta)?;
    require_meta_kind(&staged.meta)?;
    let result = ResultRecord {
        result_key,
        object_hash,
        meta_hash,
        created_at: Some(created_at.to_string()),
        inputs,
        meta: staged.meta,
    };
    write_result_record(layout, &result)?;
    store_build_handle_ref(layout, build_key, result_key)?;

    Ok(PublishedBuild {
        object_path: layout.objects.join(object_hash.to_hex()),
        build: build_from_result(build_key, &result),
        result,
    })
}

pub fn publish_refs(
    layout: &StoreLayout,
    output_name: &str,
    published: &PublishedBuild,
) -> Result<(), CasError> {
    validate_output_name(output_name)?;

    let current_meta_ref_path = layout.meta_refs.join(format!("{output_name}.json"));
    let current_object_ref_path = layout.object_refs.join(output_name);

    if let Some(current) = load_current_publication(layout, output_name)? {
        if current.result.result_key != published.result.result_key {
            let generation_name =
                allocate_generation_name(layout, output_name, &generation_suffix(&current)?)?;

            if let Some(target) = current.meta_target {
                create_generation_ref(
                    &target,
                    &layout.meta_refs.join(format!("{generation_name}.json")),
                )?;
            }
            if let Some(target) = current.object_target {
                create_generation_ref(&target, &layout.object_refs.join(&generation_name))?;
            }
        }
    }

    let object_ref_target = PathBuf::from("..")
        .join(OBJECTS_DIR)
        .join(published.build.object_hash.to_hex());
    replace_symlink(&object_ref_target, &current_object_ref_path)?;

    let meta_ref_target = PathBuf::from("..")
        .join(BUILDS_DIR)
        .join(published.build.build_key.to_hex());
    replace_symlink(&meta_ref_target, &current_meta_ref_path)?;
    Ok(())
}

pub fn object_path(layout: &StoreLayout, object_hash: ObjectHash) -> PathBuf {
    layout.objects.join(object_hash.to_hex())
}

pub fn build_ref_path(layout: &StoreLayout, build_key: BuildKey) -> PathBuf {
    layout.builds.join(build_key.to_hex())
}

pub fn result_path(layout: &StoreLayout, result_key: BuildKey) -> PathBuf {
    layout.results.join(format!("{}.json", result_key.to_hex()))
}

pub fn store_build_handle_ref(
    layout: &StoreLayout,
    build_key: BuildKey,
    result_key: BuildKey,
) -> Result<(), CasError> {
    let target = PathBuf::from("..")
        .join(RESULTS_DIR)
        .join(format!("{}.json", result_key.to_hex()));
    replace_symlink(&target, &build_ref_path(layout, build_key))
}

pub fn load_build_handle(
    layout: &StoreLayout,
    build_key: BuildKey,
) -> Result<Option<PublishedBuild>, CasError> {
    let build_ref_path = build_ref_path(layout, build_key);
    if !build_ref_path.exists() && !build_ref_path.is_symlink() {
        return Ok(None);
    }

    let target = fs::read_link(&build_ref_path).map_err(|error| {
        CasError::Io(format!(
            "failed to read build ref '{}': {error}",
            build_ref_path.display()
        ))
    })?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CasError::Serialization(format!(
                "build ref '{}' points to invalid target '{}'",
                build_ref_path.display(),
                target.display()
            ))
        })?;
    let result_key_str = file_name.strip_suffix(".json").ok_or_else(|| {
        CasError::Serialization(format!(
            "build ref '{}' points to non-JSON result target '{}'",
            build_ref_path.display(),
            target.display()
        ))
    })?;
    let result_key = parse_build_key_result(result_key_str)?;
    let result = load_result_record(layout, result_key)?.ok_or_else(|| {
        CasError::Serialization(format!(
            "build ref '{}' points to missing result '{}'",
            build_ref_path.display(),
            result_key
        ))
    })?;
    let object_path = object_path(layout, result.object_hash);
    if !object_path.exists() {
        return Err(CasError::Io(format!(
            "result '{}' points to missing object '{}'",
            result.result_key,
            object_path.display()
        )));
    }
    Ok(Some(PublishedBuild {
        build: build_from_result(build_key, &result),
        result,
        object_path,
    }))
}

pub fn load_result_record(
    layout: &StoreLayout,
    result_key: BuildKey,
) -> Result<Option<ResultRecord>, CasError> {
    let result_path = result_path(layout, result_key);
    if !result_path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&result_path).map_err(|error| {
        CasError::Io(format!(
            "failed to read result record '{}': {error}",
            result_path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        CasError::Serialization(format!(
            "failed to parse result record '{}': {error}",
            result_path.display()
        ))
    })?;
    Ok(Some(parse_result_record_value(result_key, &value)?))
}

fn build_from_result(build_key: BuildKey, result: &ResultRecord) -> Build {
    Build {
        build_key,
        object_hash: result.object_hash,
        meta_hash: result.meta_hash,
        created_at: result.created_at.clone(),
        meta: result.meta.clone(),
    }
}

pub fn import_object(layout: &StoreLayout, staged_path: &Path) -> Result<ObjectHash, CasError> {
    let object_hash = hash_path(staged_path).map_err(|error| {
        CasError::Hashing(format!(
            "failed to hash staged object '{}': {error}",
            staged_path.display()
        ))
    })?;
    let destination = layout.objects.join(object_hash.to_hex());
    if destination.exists() {
        remove_path_force(staged_path)?;
        return Ok(object_hash);
    }

    if let Err(error) = fs::rename(staged_path, &destination) {
        if destination.exists() {
            remove_path_force(staged_path)?;
            return Ok(object_hash);
        }
        return Err(CasError::Io(format!(
            "failed to import object '{}' -> '{}': {error}",
            staged_path.display(),
            destination.display()
        )));
    }

    Ok(object_hash)
}

fn write_result_record(layout: &StoreLayout, record: &ResultRecord) -> Result<(), CasError> {
    let result_value = result_record_json_value(record);
    let canonical = canonical_json_bytes(&result_value)?;
    let result_path = result_path(layout, record.result_key);
    fsutil::write_atomic(
        &result_path,
        std::str::from_utf8(&canonical).map_err(|error| {
            CasError::Serialization(format!(
                "failed to encode canonical result JSON as UTF-8: {error}"
            ))
        })?,
    )
    .map_err(map_fsutil_error)
}

fn result_json_value(
    result_key: BuildKey,
    created_at: Option<&str>,
    object_hash: ObjectHash,
    meta_hash: MetaHash,
    inputs: &[ResultInputIdentity],
    meta: Map<String, Value>,
) -> Value {
    let input_values = inputs
        .iter()
        .map(|input| {
            let mut object = Map::new();
            object.insert(
                "object_hash".to_string(),
                Value::String(input.object_hash.to_string()),
            );
            object.insert(
                "meta_hash".to_string(),
                Value::String(input.meta_hash.to_string()),
            );
            Value::Object(object)
        })
        .collect::<Vec<_>>();

    let mut root = Map::new();
    root.insert(
        "schema".to_string(),
        Value::String(RESULT_SCHEMA.to_string()),
    );
    root.insert(
        "result_key".to_string(),
        Value::String(result_key.to_string()),
    );
    if let Some(created_at) = created_at {
        root.insert(
            "created_at".to_string(),
            Value::String(created_at.to_string()),
        );
    }
    root.insert(
        "object_hash".to_string(),
        Value::String(object_hash.to_string()),
    );
    root.insert(
        "meta_hash".to_string(),
        Value::String(meta_hash.to_string()),
    );
    root.insert("inputs".to_string(), Value::Array(input_values));
    root.insert("meta".to_string(), Value::Object(meta));
    Value::Object(root)
}

fn result_record_json_value(record: &ResultRecord) -> Value {
    result_json_value(
        record.result_key,
        record.created_at.as_deref(),
        record.object_hash,
        record.meta_hash,
        &record.inputs,
        record.meta.clone(),
    )
}

#[cfg(test)]
fn build_json_value(
    result_key: BuildKey,
    created_at: Option<&str>,
    object_hash: ObjectHash,
    meta_hash: MetaHash,
    inputs: &[ResultInputIdentity],
    meta: Map<String, Value>,
) -> Value {
    result_json_value(result_key, created_at, object_hash, meta_hash, inputs, meta)
}

fn parse_result_record_value(
    result_key: BuildKey,
    value: &Value,
) -> Result<ResultRecord, CasError> {
    let object = value.as_object().ok_or_else(|| {
        CasError::Serialization("result record root must be a JSON object".to_string())
    })?;

    let schema = object
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("result record is missing 'schema'".to_string()))?;
    if schema != RESULT_SCHEMA {
        return Err(CasError::Serialization(format!(
            "unsupported result record schema '{schema}'"
        )));
    }

    let encoded_result_key = object
        .get("result_key")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("result record is missing 'result_key'".to_string()))
        .and_then(parse_build_key_result)?;
    if encoded_result_key != result_key {
        return Err(CasError::Serialization(format!(
            "result record key mismatch: path key '{}' does not match encoded key '{}'",
            result_key, encoded_result_key
        )));
    }

    let created_at = object
        .get("created_at")
        .map(|value| {
            value.as_str().ok_or_else(|| {
                CasError::Serialization("result record created_at must be a string".to_string())
            })
        })
        .transpose()?
        .map(str::to_string);

    let object_hash = object
        .get("object_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CasError::Serialization("result record is missing 'object_hash'".to_string())
        })
        .and_then(parse_object_hash_result)?;

    let meta_hash = object
        .get("meta_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| CasError::Serialization("result record is missing 'meta_hash'".to_string()))
        .and_then(parse_meta_hash_result)?;

    let inputs = object
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| CasError::Serialization("result record is missing 'inputs'".to_string()))?
        .iter()
        .map(|value| {
            let object = value.as_object().ok_or_else(|| {
                CasError::Serialization("result record inputs must contain objects".to_string())
            })?;
            let object_hash = object
                .get("object_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CasError::Serialization(
                        "result record input is missing 'object_hash'".to_string(),
                    )
                })
                .and_then(parse_object_hash_result)?;
            let meta_hash = object
                .get("meta_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CasError::Serialization(
                        "result record input is missing 'meta_hash'".to_string(),
                    )
                })
                .and_then(parse_meta_hash_result)?;
            Ok(ResultInputIdentity {
                object_hash,
                meta_hash,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let meta = object
        .get("meta")
        .and_then(Value::as_object)
        .ok_or_else(|| CasError::Serialization("result record is missing 'meta'".to_string()))?
        .clone();
    require_meta_kind(&meta)?;

    Ok(ResultRecord {
        result_key,
        object_hash,
        meta_hash,
        created_at,
        inputs,
        meta,
    })
}

fn parse_object_hash_result(value: &str) -> Result<ObjectHash, CasError> {
    value.parse::<ObjectHash>().map_err(|error| {
        CasError::Serialization(format!(
            "invalid object hash '{value}' in build record: {error}"
        ))
    })
}

fn parse_meta_hash_result(value: &str) -> Result<MetaHash, CasError> {
    value.parse::<MetaHash>().map_err(|error| {
        CasError::Serialization(format!(
            "invalid meta hash '{value}' in build record: {error}"
        ))
    })
}

fn parse_build_key_result(value: &str) -> Result<BuildKey, CasError> {
    value.parse::<BuildKey>().map_err(|error| {
        CasError::Serialization(format!(
            "invalid build key '{value}' in build record: {error}"
        ))
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

#[derive(Debug)]
struct CurrentPublication {
    result: ResultRecord,
    result_path: PathBuf,
    meta_target: Option<PathBuf>,
    object_target: Option<PathBuf>,
}

fn load_current_publication(
    layout: &StoreLayout,
    output_name: &str,
) -> Result<Option<CurrentPublication>, CasError> {
    let meta_ref_path = layout.meta_refs.join(format!("{output_name}.json"));
    if !meta_ref_path.exists() && !meta_ref_path.is_symlink() {
        return Ok(None);
    }

    let meta_target = fs::read_link(&meta_ref_path).map_err(|error| {
        CasError::Io(format!(
            "failed to read current meta ref '{}': {error}",
            meta_ref_path.display()
        ))
    })?;
    let meta_file_name = meta_target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CasError::Serialization(format!(
                "current meta ref '{}' points to invalid target '{}'",
                meta_ref_path.display(),
                meta_target.display()
            ))
        })?;
    let build_key_str = meta_file_name;
    let build_key = parse_build_key_result(build_key_str)?;
    let published = load_build_handle(layout, build_key)?.ok_or_else(|| {
        CasError::Serialization(format!(
            "current meta ref '{}' points to missing build '{}'",
            meta_ref_path.display(),
            build_key
        ))
    })?;

    let object_ref_path = layout.object_refs.join(output_name);
    let object_target = if object_ref_path.exists() || object_ref_path.is_symlink() {
        Some(fs::read_link(&object_ref_path).map_err(|error| {
            CasError::Io(format!(
                "failed to read current object ref '{}': {error}",
                object_ref_path.display()
            ))
        })?)
    } else {
        None
    };

    Ok(Some(CurrentPublication {
        result_path: result_path(layout, published.result.result_key),
        result: published.result,
        meta_target: Some(meta_target),
        object_target,
    }))
}

fn generation_suffix(current: &CurrentPublication) -> Result<String, CasError> {
    if let Some(created_at) = &current.result.created_at {
        return human_timestamp_from_rfc3339(created_at);
    }

    let modified = fs::metadata(&current.result_path)
        .map_err(|error| {
            CasError::Io(format!(
                "failed to stat result record '{}' for generation timestamp: {error}",
                current.result_path.display()
            ))
        })?
        .modified()
        .map_err(|error| {
            CasError::Io(format!(
                "failed to read mtime for result record '{}': {error}",
                current.result_path.display()
            ))
        })?;
    let parsed = OffsetDateTime::from(modified);
    human_timestamp_from_datetime(parsed)
}

fn human_timestamp_from_rfc3339(value: &str) -> Result<String, CasError> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        CasError::Serialization(format!(
            "invalid build record created_at '{value}': {error}"
        ))
    })?;
    human_timestamp_from_datetime(parsed)
}

fn human_timestamp_from_datetime(parsed: OffsetDateTime) -> Result<String, CasError> {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local = parsed.to_offset(offset);
    let format = format_description!("[year repr:last_two][month][day][hour][minute][second]");
    local.format(&format).map_err(|error| {
        CasError::Serialization(format!("failed to format generation suffix: {error}"))
    })
}

fn allocate_generation_name(
    layout: &StoreLayout,
    output_name: &str,
    suffix: &str,
) -> Result<String, CasError> {
    for counter in 1..1000 {
        let candidate = if counter == 1 {
            format!("{output_name}.{suffix}")
        } else {
            format!("{output_name}.{suffix}.{counter}")
        };
        let meta_path = layout.meta_refs.join(format!("{candidate}.json"));
        let object_path = layout.object_refs.join(&candidate);
        if !(meta_path.exists()
            || meta_path.is_symlink()
            || object_path.exists()
            || object_path.is_symlink())
        {
            return Ok(candidate);
        }
    }

    Err(CasError::Io(format!(
        "failed to allocate generation ref name for '{output_name}.{suffix}'"
    )))
}

fn create_generation_ref(target: &Path, link_path: &Path) -> Result<(), CasError> {
    if link_path.exists() || link_path.is_symlink() {
        return Err(CasError::Io(format!(
            "ref generation collision at '{}'",
            link_path.display()
        )));
    }
    create_symlink(target, link_path)
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

    fn with_kind(kind: &str, mut meta: Map<String, Value>) -> Map<String, Value> {
        meta.insert("kind".to_string(), Value::String(kind.to_string()));
        meta
    }

    #[test]
    fn canonical_json_hash_is_stable_across_key_order() {
        let build_key =
            parse_build_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut meta_a = Map::new();
        meta_a.insert("z".to_string(), Value::from(1));
        meta_a.insert("a".to_string(), Value::from(true));
        let left = build_json_value(
            build_key,
            Some(sample_created_at()),
            parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111"),
            compute_meta_hash(&meta_a).unwrap(),
            &[],
            meta_a,
        );

        let mut meta_b = Map::new();
        meta_b.insert("a".to_string(), Value::from(true));
        meta_b.insert("z".to_string(), Value::from(1));
        let right = build_json_value(
            build_key,
            Some(sample_created_at()),
            parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111"),
            compute_meta_hash(&meta_b).unwrap(),
            &[],
            meta_b,
        );

        assert_eq!(
            canonical_json_bytes(&left).unwrap(),
            canonical_json_bytes(&right).unwrap()
        );
    }

    #[test]
    fn meta_hash_is_stable_across_key_order() {
        let mut left = Map::new();
        left.insert("z".to_string(), Value::from(1));
        left.insert("a".to_string(), Value::from(true));

        let mut right = Map::new();
        right.insert("a".to_string(), Value::from(true));
        right.insert("z".to_string(), Value::from(1));

        assert_eq!(
            compute_meta_hash(&left).unwrap(),
            compute_meta_hash(&right).unwrap()
        );
    }

    #[test]
    fn result_key_changes_when_input_meta_hash_changes() {
        let payload = json!({ "kind": "build-script" });
        let object_hash =
            parse_object_hash("1111111111111111111111111111111111111111111111111111111111111111");
        let left = [ResultInputIdentity {
            object_hash,
            meta_hash: parse_meta_hash(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        }];
        let right = [ResultInputIdentity {
            object_hash,
            meta_hash: parse_meta_hash(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        }];

        assert_ne!(
            compute_result_key("Text", &payload, &left).unwrap(),
            compute_result_key("Text", &payload, &right).unwrap()
        );
    }

    #[test]
    fn result_key_is_stable_for_identical_inputs() {
        let payload = json!({ "kind": "build-script" });
        let inputs = vec![
            ResultInputIdentity {
                object_hash: parse_object_hash(
                    "1111111111111111111111111111111111111111111111111111111111111111",
                ),
                meta_hash: parse_meta_hash(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            },
            ResultInputIdentity {
                object_hash: parse_object_hash(
                    "2222222222222222222222222222222222222222222222222222222222222222",
                ),
                meta_hash: parse_meta_hash(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                ),
            },
        ];

        assert_eq!(
            compute_result_key("Text", &payload, &inputs).unwrap(),
            compute_result_key("Text", &payload, &inputs).unwrap()
        );
    }

    #[test]
    fn meta_hash_changes_when_meta_changes() {
        let left = Map::from_iter([
            ("source_bytes".to_string(), Value::from(5)),
            ("generated".to_string(), Value::from(false)),
        ]);
        let right = Map::from_iter([
            ("source_bytes".to_string(), Value::from(6)),
            ("generated".to_string(), Value::from(false)),
        ]);

        assert_ne!(
            compute_meta_hash(&left).unwrap(),
            compute_meta_hash(&right).unwrap()
        );
    }

    #[test]
    fn parse_result_record_rejects_old_schema() {
        let result_key =
            parse_build_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let value = json!({
            "schema": "mbuild-result-v1",
            "result_key": result_key.to_string(),
            "kind": "build-script",
            "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
            "meta_hash": "2222222222222222222222222222222222222222222222222222222222222222",
            "producer": { "builder": "text" },
            "inputs": [],
            "meta": {},
        });

        assert!(matches!(
            parse_result_record_value(result_key, &value),
            Err(CasError::Serialization(message))
                if message == "unsupported result record schema 'mbuild-result-v1'"
        ));
    }

    #[test]
    fn publish_output_reuses_existing_result_via_new_build_handle_ref() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let result_key = build_key_for("Text", json!({ "kind": "build-script" }), &[]);
        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "hello".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello-1" }),
                    &[],
                ),
                result_key,
                created_at: sample_created_at().to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind(
                    "build-script",
                    Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
                ),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "hello-copy".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello-2" }),
                    &[],
                ),
                result_key,
                created_at: sample_created_at().to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind(
                    "build-script",
                    Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
                ),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
        assert_eq!(first.result_key, second.result_key);
        assert!(layout.objects.join(first.object_hash.to_hex()).exists());
        assert_eq!(
            fs::read_link(layout.builds.join(first.build_key.to_hex())).unwrap(),
            PathBuf::from("..")
                .join(RESULTS_DIR)
                .join(format!("{}.json", first.result_key.to_hex()))
        );
        assert!(layout.builds.join(second.build_key.to_hex()).exists());
        assert_eq!(
            fs::read_link(layout.builds.join(second.build_key.to_hex())).unwrap(),
            PathBuf::from("..")
                .join(RESULTS_DIR)
                .join(format!("{}.json", second.result_key.to_hex()))
        );
        assert_eq!(
            fs::read_link(layout.meta_refs.join("hello-copy.json")).unwrap(),
            PathBuf::from("..")
                .join(BUILDS_DIR)
                .join(second.build_key.to_hex())
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
                        "1111111111111111111111111111111111111111111111111111111111111111",
                    )],
                ),
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "echo hi\n" }),
                    &[],
                ),
                created_at: sample_created_at().to_string(),
                staged_path: stage,
                inputs: vec![],
                meta: with_kind(
                    "build-script",
                    Map::from_iter([
                        ("source_bytes".to_string(), Value::from(8)),
                        ("generated".to_string(), Value::from(false)),
                    ]),
                ),
            },
        )
        .unwrap();

        let build_ref_path = layout.builds.join(published.build_key.to_hex());
        let result_path = layout
            .results
            .join(format!("{}.json", published.result_key.to_hex()));
        assert!(build_ref_path.exists());
        assert!(result_path.exists());
        assert_eq!(
            fs::read_link(&build_ref_path).unwrap(),
            PathBuf::from("..")
                .join(RESULTS_DIR)
                .join(format!("{}.json", published.result_key.to_hex()))
        );

        let build_json: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        assert_eq!(
            build_json["schema"],
            Value::String(BUILD_SCHEMA.to_string())
        );
        assert_eq!(
            build_json["result_key"],
            Value::String(published.result_key.to_string())
        );
        assert_eq!(
            build_json["created_at"],
            Value::String(sample_created_at().to_string())
        );
        assert_eq!(
            build_json["object_hash"],
            Value::String(published.object_hash.to_string())
        );
        assert_eq!(
            build_json["meta_hash"],
            Value::String(
                compute_meta_hash(&Map::from_iter([
                    (
                        "kind".to_string(),
                        Value::String("build-script".to_string())
                    ),
                    ("source_bytes".to_string(), Value::from(8)),
                    ("generated".to_string(), Value::from(false)),
                ]))
                .unwrap()
                .to_string()
            )
        );
        assert_eq!(build_json["inputs"], Value::Array(vec![]));
        assert_eq!(build_json["meta"]["kind"], Value::from("build-script"));
        assert_eq!(build_json["meta"]["source_bytes"], Value::from(8));
        assert_eq!(build_json["meta"]["generated"], Value::from(false));

        assert_eq!(
            fs::read_link(layout.meta_refs.join("script.json")).unwrap(),
            PathBuf::from("..")
                .join(BUILDS_DIR)
                .join(published.build_key.to_hex())
        );
        assert_eq!(
            fs::read_link(layout.object_refs.join("script")).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(published.object_hash.to_hex())
        );
    }

    #[test]
    fn result_record_round_trips_inputs_meta_hash_and_meta() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let meta = with_kind(
            "build-script",
            Map::from_iter([
                ("source_bytes".to_string(), Value::from(8)),
                ("generated".to_string(), Value::from(false)),
            ]),
        );
        let inputs = vec![
            ResultInputIdentity {
                object_hash: parse_object_hash(
                    "1111111111111111111111111111111111111111111111111111111111111111",
                ),
                meta_hash: parse_meta_hash(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            },
            ResultInputIdentity {
                object_hash: parse_object_hash(
                    "2222222222222222222222222222222222222222222222222222222222222222",
                ),
                meta_hash: parse_meta_hash(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                ),
            },
        ];
        let expected_meta_hash = compute_meta_hash(&meta).unwrap();
        let result_key = compute_result_key(
            "Text",
            &json!({ "kind": "build-script", "source": "echo hi\n" }),
            &inputs,
        )
        .unwrap();

        let stage = temp.path().join("script.sh");
        fs::write(&stage, b"echo hi\n").unwrap();
        let published = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "script".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "echo hi\n" }),
                    &[],
                ),
                result_key,
                created_at: sample_created_at().to_string(),
                staged_path: stage,
                inputs: inputs.clone(),
                meta: meta.clone(),
            },
        )
        .unwrap();

        let loaded = load_result_record(&layout, published.result_key)
            .unwrap()
            .expect("expected result record to exist");

        assert_eq!(loaded.inputs, inputs);
        assert_eq!(loaded.meta_hash, expected_meta_hash);
        assert_eq!(loaded.meta, meta);
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
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello" }),
                    &[],
                ),
                created_at: sample_created_at().to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind(
                    "build-script",
                    Map::from_iter([(String::from("source_bytes"), Value::from(5))]),
                ),
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
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "source-tree", "source": "hello" }),
                    &[],
                ),
                created_at: sample_created_at().to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind(
                    "source-tree",
                    Map::from_iter([(String::from("source_bytes"), Value::from(6))]),
                ),
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
                result_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
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
                result_key: build_key_for("Text", json!({ "kind": "source-tree" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind("source-tree", Map::new()),
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
                result_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
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
                result_key: build_key_for("Fetch", json!({ "kind": "build-script" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        assert_eq!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
    }

    #[test]
    fn publish_output_rotates_existing_refs_into_generations() {
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
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello" }),
                    &[],
                ),
                created_at: "2026-03-24T12:34:56.123456789Z".to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
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
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello world" }),
                    &[],
                ),
                created_at: "2026-03-24T12:35:30.123456789Z".to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        let suffix = human_timestamp_from_rfc3339("2026-03-24T12:34:56.123456789Z").unwrap();
        assert_ne!(first.object_hash, second.object_hash);
        assert_ne!(first.build_key, second.build_key);
        assert_eq!(
            fs::read_link(layout.object_refs.join("shared")).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(second.object_hash.to_hex())
        );
        assert_eq!(
            fs::read_link(layout.meta_refs.join("shared.json")).unwrap(),
            PathBuf::from("..")
                .join(BUILDS_DIR)
                .join(second.build_key.to_hex())
        );
        assert_eq!(
            fs::read_link(layout.object_refs.join(format!("shared.{suffix}"))).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(first.object_hash.to_hex())
        );
        assert_eq!(
            fs::read_link(layout.meta_refs.join(format!("shared.{suffix}.json"))).unwrap(),
            PathBuf::from("..")
                .join(BUILDS_DIR)
                .join(first.build_key.to_hex())
        );
    }

    #[test]
    fn publish_output_same_build_key_does_not_create_generation_refs() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"hello").unwrap();
        let build_key = build_key_for(
            "Text",
            json!({ "kind": "build-script", "source": "hello" }),
            &[],
        );
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "shared".to_string(),
                build_key,
                result_key: build_key,
                created_at: "2026-03-24T12:34:56.123456789Z".to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"hello").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "shared".to_string(),
                build_key,
                result_key: build_key,
                created_at: "2026-03-24T12:35:30.123456789Z".to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        assert_eq!(first.build_key, second.build_key);
        assert_eq!(first.object_hash, second.object_hash);
        let suffix = human_timestamp_from_rfc3339("2026-03-24T12:34:56.123456789Z").unwrap();
        assert!(!layout.object_refs.join(format!("shared.{suffix}")).exists());
        assert!(
            !layout
                .meta_refs
                .join(format!("shared.{suffix}.json"))
                .exists()
        );
    }

    #[test]
    fn publish_output_generation_suffix_collisions_get_numeric_suffixes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();

        let first_stage = temp.path().join("first.txt");
        fs::write(&first_stage, b"one").unwrap();
        let first = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "shared".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "one" }),
                    &[],
                ),
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "one" }),
                    &[],
                ),
                created_at: "2026-03-24T12:34:56.100000000Z".to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        let second_stage = temp.path().join("second.txt");
        fs::write(&second_stage, b"two").unwrap();
        let second = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "shared".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "two" }),
                    &[],
                ),
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "two" }),
                    &[],
                ),
                created_at: "2026-03-24T12:34:56.200000000Z".to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        let third_stage = temp.path().join("third.txt");
        fs::write(&third_stage, b"three").unwrap();
        let third = publish_output(
            &layout,
            PublishOutputRequest {
                output_name: "shared".to_string(),
                build_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "three" }),
                    &[],
                ),
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "three" }),
                    &[],
                ),
                created_at: "2026-03-24T12:34:56.300000000Z".to_string(),
                staged_path: third_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        let suffix = human_timestamp_from_rfc3339("2026-03-24T12:34:56.100000000Z").unwrap();
        assert_eq!(
            fs::read_link(layout.object_refs.join(format!("shared.{suffix}"))).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(first.object_hash.to_hex())
        );
        assert_eq!(
            fs::read_link(layout.object_refs.join(format!("shared.{suffix}.2"))).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(second.object_hash.to_hex())
        );
        assert_eq!(
            fs::read_link(layout.object_refs.join("shared")).unwrap(),
            PathBuf::from("..")
                .join(OBJECTS_DIR)
                .join(third.object_hash.to_hex())
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
                    result_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                    created_at: sample_created_at().to_string(),
                    staged_path: stage,
                    inputs: vec![],
                    meta: with_kind("build-script", Map::new()),
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
                result_key: build_key_for("Fetch", json!({ "kind": "source-tree" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: stage_dir,
                inputs: vec![],
                meta: with_kind("source-tree", Map::new()),
            },
        )
        .unwrap();

        let object_path = layout.objects.join(published.object_hash.to_hex());
        assert!(object_path.is_dir());
        assert!(object_path.join("bin").join("tool").exists());
        assert!(layout.builds.join(published.build_key.to_hex()).exists());
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
                result_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
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
                result_key: build_key_for("Text", json!({ "kind": "build-script" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
            },
        )
        .unwrap();

        assert!(!second_stage_path.exists());
    }

    #[test]
    fn build_key_changes_when_input_build_key_order_changes() {
        let temp = tempdir().unwrap();
        let layout = StoreLayout::discover(&temp.path().join(".mbuild")).unwrap();
        let key_a =
            parse_build_key("1111111111111111111111111111111111111111111111111111111111111111");
        let key_b =
            parse_build_key("2222222222222222222222222222222222222222222222222222222222222222");

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
                result_key: build_key_for("Binary", json!({ "kind": "binary-output" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("binary-output", Map::new()),
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
                result_key: build_key_for("Binary", json!({ "kind": "binary-output" }), &[]),
                created_at: sample_created_at().to_string(),
                staged_path: second_stage,
                inputs: vec![],
                meta: with_kind("binary-output", Map::new()),
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
        let key =
            BuildKey::from_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .unwrap();

        assert_eq!(
            key.to_string(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
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
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello", "variant": "plain" }),
                    &[],
                ),
                created_at: sample_created_at().to_string(),
                staged_path: first_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
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
                result_key: build_key_for(
                    "Text",
                    json!({ "kind": "build-script", "source": "hello", "variant": "exec" }),
                    &[],
                ),
                created_at: sample_created_at().to_string(),
                staged_path: exec_stage,
                inputs: vec![],
                meta: with_kind("build-script", Map::new()),
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

    fn parse_meta_hash(value: &str) -> MetaHash {
        MetaHash::from_str(value).unwrap()
    }

    fn sample_created_at() -> &'static str {
        "2026-03-24T12:34:56.123456789Z"
    }

    fn build_key_for(builder_tag: &str, payload: Value, input_build_keys: &[BuildKey]) -> BuildKey {
        compute_build_key(builder_tag, &payload, input_build_keys).unwrap()
    }
}
