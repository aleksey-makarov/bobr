use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const MEDIA_TYPE_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_TYPE_OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const MEDIA_TYPE_OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

#[derive(Debug)]
pub enum OciError {
    Io(String),
    Parse(String),
}

impl fmt::Display for OciError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OciError::Io(msg) | OciError::Parse(msg) => f.write_str(msg),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OciLayout {
    #[serde(rename = "imageLayoutVersion")]
    pub image_layout_version: String,
}

impl OciLayout {
    pub fn v1() -> Self {
        Self {
            image_layout_version: "1.0.0".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciDescriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<OciPlatform>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciPlatform {
    pub os: String,
    pub architecture: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OciIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciManifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub config: OciDescriptor,
    pub layers: Vec<OciDescriptor>,
}

pub fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("{hash:x}")
}

pub fn sha256_digest(data: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(data))
}

/// Path to a blob inside an OCI layout directory.
/// digest must be "sha256:<hex64>".
pub fn blob_path(oci_dir: &Path, digest: &str) -> PathBuf {
    let (alg, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
    oci_dir.join("blobs").join(alg).join(hex)
}

/// Read index.json and return the first manifest descriptor.
pub fn read_index(oci_dir: &Path) -> Result<OciDescriptor, OciError> {
    let path = oci_dir.join("index.json");
    let bytes = fs::read(&path)
        .map_err(|e| OciError::Io(format!("failed to read index.json '{}': {e}", path.display())))?;
    let index: OciIndex = serde_json::from_slice(&bytes)
        .map_err(|e| OciError::Parse(format!("failed to parse index.json: {e}")))?;
    index
        .manifests
        .into_iter()
        .next()
        .ok_or_else(|| OciError::Parse("index.json contains no manifests".to_string()))
}

/// Read a manifest blob from an OCI layout directory.
pub fn read_manifest(oci_dir: &Path, descriptor: &OciDescriptor) -> Result<OciManifest, OciError> {
    let path = blob_path(oci_dir, &descriptor.digest);
    let bytes = fs::read(&path).map_err(|e| {
        OciError::Io(format!(
            "failed to read manifest blob '{}': {e}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| OciError::Parse(format!("failed to parse manifest: {e}")))
}

/// Read and return the OCI manifest for the first entry in index.json.
pub fn read_oci_manifest(oci_dir: &Path) -> Result<OciManifest, OciError> {
    let desc = read_index(oci_dir)?;
    read_manifest(oci_dir, &desc)
}

/// Read the config blob as a JSON Value (to preserve unknown fields).
pub fn read_config(oci_dir: &Path, manifest: &OciManifest) -> Result<Value, OciError> {
    let path = blob_path(oci_dir, &manifest.config.digest);
    let bytes = fs::read(&path).map_err(|e| {
        OciError::Io(format!(
            "failed to read config blob '{}': {e}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| OciError::Parse(format!("failed to parse config blob: {e}")))
}

/// Write a blob to an OCI layout directory and return its descriptor.
pub fn write_blob(
    oci_dir: &Path,
    data: &[u8],
    media_type: impl Into<String>,
) -> Result<OciDescriptor, OciError> {
    let digest = sha256_digest(data);
    let path = blob_path(oci_dir, &digest);
    fs::write(&path, data).map_err(|e| {
        OciError::Io(format!(
            "failed to write blob '{}': {e}",
            path.display()
        ))
    })?;
    Ok(OciDescriptor {
        media_type: media_type.into(),
        digest,
        size: data.len() as u64,
        platform: None,
        annotations: None,
    })
}

/// Initialize a new OCI layout directory (creates oci-layout, blobs/sha256/).
pub fn init_layout(oci_dir: &Path) -> Result<(), OciError> {
    fs::create_dir_all(oci_dir.join("blobs").join("sha256")).map_err(|e| {
        OciError::Io(format!(
            "failed to create blobs dir in '{}': {e}",
            oci_dir.display()
        ))
    })?;
    let layout_bytes = serde_json::to_vec(&OciLayout::v1()).unwrap();
    fs::write(oci_dir.join("oci-layout"), layout_bytes)
        .map_err(|e| OciError::Io(format!("failed to write oci-layout: {e}")))?;
    Ok(())
}

/// Write index.json referencing a single manifest descriptor.
/// If `image_ref` is provided, sets the `org.opencontainers.image.ref.name` annotation
/// so that `podman load` assigns a recognizable name to the image.
pub fn write_index(oci_dir: &Path, mut manifest_desc: OciDescriptor, image_ref: Option<&str>) -> Result<(), OciError> {
    if let Some(name) = image_ref {
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert("org.opencontainers.image.ref.name".to_string(), name.to_string());
        manifest_desc.annotations = Some(annotations);
    }
    let index = OciIndex {
        schema_version: 2,
        manifests: vec![manifest_desc],
    };
    let bytes = serde_json::to_vec_pretty(&index)
        .map_err(|e| OciError::Io(format!("failed to serialize index.json: {e}")))?;
    fs::write(oci_dir.join("index.json"), bytes)
        .map_err(|e| OciError::Io(format!("failed to write index.json: {e}")))?;
    Ok(())
}

/// Hardlink only the layer blobs from src_oci_dir into dst_oci_dir.
/// Reads the base manifest to find layer digests; skips base config and manifest blobs.
/// dst_oci_dir must already have blobs/sha256/ created.
pub fn hardlink_layer_blobs(src_oci_dir: &Path, dst_oci_dir: &Path) -> Result<(), OciError> {
    let manifest = read_oci_manifest(src_oci_dir)?;
    let dst_blobs = dst_oci_dir.join("blobs").join("sha256");
    for layer in &manifest.layers {
        let src = blob_path(src_oci_dir, &layer.digest);
        let (_, hex) = layer.digest.split_once(':').unwrap_or(("sha256", &layer.digest));
        let dst = dst_blobs.join(hex);
        fs::hard_link(&src, &dst).map_err(|e| {
            OciError::Io(format!(
                "failed to hardlink '{}' -> '{}': {e}",
                src.display(),
                dst.display()
            ))
        })?;
    }
    Ok(())
}
