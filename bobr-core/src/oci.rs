use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Media type of an OCI image manifest.
pub const MEDIA_TYPE_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// Media type of an OCI image config.
pub const MEDIA_TYPE_OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
/// Media type of a gzipped OCI image layer.
pub const MEDIA_TYPE_OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// Error reading or parsing an OCI image layout.
#[derive(Debug)]
pub enum OciError {
    /// Filesystem I/O error.
    Io(String),
    /// JSON parse or unexpected-structure error.
    Parse(String),
}

impl fmt::Display for OciError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OciError::Io(msg) | OciError::Parse(msg) => f.write_str(msg),
        }
    }
}

/// Contents of the `oci-layout` marker file.
#[derive(Debug, Serialize, Deserialize)]
pub struct OciLayout {
    /// The layout version string (e.g. `"1.0.0"`).
    #[serde(rename = "imageLayoutVersion")]
    pub image_layout_version: String,
}

impl OciLayout {
    /// The version `1.0.0` layout marker.
    pub fn v1() -> Self {
        Self {
            image_layout_version: "1.0.0".to_string(),
        }
    }
}

/// An OCI content descriptor: what some content is, its digest, and its size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciDescriptor {
    /// Media type of the referenced content.
    #[serde(rename = "mediaType")]
    pub media_type: String,
    /// Content digest (e.g. `sha256:<hex>`).
    pub digest: String,
    /// Content size, in bytes.
    pub size: u64,
    /// Target platform, for multi-platform index entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<OciPlatform>,
    /// Optional annotations (e.g. the image ref name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<std::collections::BTreeMap<String, String>>,
}

/// A target platform: operating system and CPU architecture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciPlatform {
    /// Operating system (e.g. `"linux"`).
    pub os: String,
    /// CPU architecture (e.g. `"amd64"`).
    pub architecture: String,
}

/// An OCI image index (`index.json`): the list of top-level manifests.
#[derive(Debug, Serialize, Deserialize)]
pub struct OciIndex {
    /// OCI schema version (`2`).
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// The manifest descriptors.
    pub manifests: Vec<OciDescriptor>,
}

/// An OCI image manifest: its config descriptor and ordered layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciManifest {
    /// OCI schema version (`2`).
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// Descriptor of the image config blob.
    pub config: OciDescriptor,
    /// Ordered layer descriptors.
    pub layers: Vec<OciDescriptor>,
}

/// Hex-encoded SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("{hash:x}")
}

/// SHA-256 of `data` as an OCI digest string (`"sha256:<hex>"`).
pub fn sha256_digest(data: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(data))
}

/// Path to a blob in an OCI layout at `oci_dir`, given its `digest`.
pub fn blob_path(oci_dir: &Path, digest: &str) -> PathBuf {
    let (alg, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
    oci_dir.join("blobs").join(alg).join(hex)
}

/// Reads `index.json` from an OCI layout and returns its first manifest
/// descriptor.
pub fn read_index(oci_dir: &Path) -> Result<OciDescriptor, OciError> {
    let path = oci_dir.join("index.json");
    let bytes = fs::read(&path).map_err(|e| {
        OciError::Io(format!(
            "failed to read index.json '{}': {e}",
            path.display()
        ))
    })?;
    let index: OciIndex = serde_json::from_slice(&bytes)
        .map_err(|e| OciError::Parse(format!("failed to parse index.json: {e}")))?;
    index
        .manifests
        .into_iter()
        .next()
        .ok_or_else(|| OciError::Parse("index.json contains no manifests".to_string()))
}

/// Reads and parses the manifest blob named by `descriptor`.
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

/// Reads the image's manifest: resolves `index.json`, then reads that manifest
/// blob.
pub fn read_oci_manifest(oci_dir: &Path) -> Result<OciManifest, OciError> {
    let desc = read_index(oci_dir)?;
    read_manifest(oci_dir, &desc)
}

/// Reads and parses the image config blob referenced by `manifest`.
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

/// Writes `data` as a content-addressed blob and returns its descriptor tagged
/// with `media_type`.
pub fn write_blob(
    oci_dir: &Path,
    data: &[u8],
    media_type: impl Into<String>,
) -> Result<OciDescriptor, OciError> {
    let digest = sha256_digest(data);
    let path = blob_path(oci_dir, &digest);
    fs::write(&path, data)
        .map_err(|e| OciError::Io(format!("failed to write blob '{}': {e}", path.display())))?;
    Ok(OciDescriptor {
        media_type: media_type.into(),
        digest,
        size: data.len() as u64,
        platform: None,
        annotations: None,
    })
}

/// Initializes an empty OCI layout at `oci_dir`: the `blobs/sha256` directory
/// and the `oci-layout` marker file.
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

/// Writes `index.json` pointing at `manifest_desc`, optionally annotating it
/// with an image `ref.name` from `image_ref`.
pub fn write_index(
    oci_dir: &Path,
    mut manifest_desc: OciDescriptor,
    image_ref: Option<&str>,
) -> Result<(), OciError> {
    if let Some(name) = image_ref {
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert(
            "org.opencontainers.image.ref.name".to_string(),
            name.to_string(),
        );
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

/// Hard-links the layer blobs of the image in `src_oci_dir` into the blob store
/// of `dst_oci_dir`.
pub fn hardlink_layer_blobs(src_oci_dir: &Path, dst_oci_dir: &Path) -> Result<(), OciError> {
    let manifest = read_oci_manifest(src_oci_dir)?;
    let dst_blobs = dst_oci_dir.join("blobs").join("sha256");
    for layer in &manifest.layers {
        let src = blob_path(src_oci_dir, &layer.digest);
        let (_, hex) = layer
            .digest
            .split_once(':')
            .unwrap_or(("sha256", &layer.digest));
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
