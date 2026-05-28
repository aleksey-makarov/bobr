use flate2::read::GzDecoder;
use fsobj_hash::{hash_file_bytes, hash_symlink_node};
use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    FsTreeEntry, FsTreeManifest, FsTreeObjectError, FsTreeObjectPaths, ObjectHash,
    StagedBuildResult, TypedBuilder, create_fs_tree_staging_dir,
    hash_fs_tree_object_from_manifest_with_extra_files,
};
#[cfg(test)]
use mbuild_core::{FsTreeOwnerMap, ValidatedFsTreeObject, validate_fs_tree_object};
use mbuild_origin_oci_registry::oci::{self, OciDescriptor, OciManifest};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use tar::{Archive, EntryType};
use tracing::warn;

const OCI_CONFIG_FILE_NAME: &str = "oci-config.json";
const OUTPUT_DIR_NAME: &str = "oci-extract";
const EXTRACT_ROOT_DIR_NAME: &str = "oci-extract-root";
const MEDIA_TYPE_DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const MEDIA_TYPE_OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciExtractConfig {}

pub struct OciExtractBuilder;

static OCI_EXTRACT_SPEC: BuilderSpec = BuilderSpec {
    tag: "OciExtract",
    required_inputs: &["image"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for OciExtractBuilder {
    type Config = OciExtractConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &OCI_EXTRACT_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        self.build_with_materializer(config, inputs, cx, &RuntimeOwnershipMaterializer)
    }
}

impl OciExtractBuilder {
    fn build_with_materializer(
        &self,
        _config: OciExtractConfig,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
        materializer: &impl OwnershipMaterializer,
    ) -> Result<StagedBuildResult, BuilderError> {
        let image = inputs.required("image")?;
        validate_oci_layout_input(image).map_err(map_error)?;

        let staged = cx.temp_dir.join(OUTPUT_DIR_NAME);
        let extract_root = cx.temp_dir.join(EXTRACT_ROOT_DIR_NAME);
        if staged.exists() || extract_root.exists() {
            return Err(map_error(OciExtractError::InvalidInput(format!(
                "OciExtract staging paths already exist under '{}'",
                cx.temp_dir.display()
            ))));
        }

        cx.log_event(
            BuildLogLevel::Info,
            "extract",
            format!("extracting OCI image '{}'", image.object_path.display()),
        );

        fs::create_dir(&extract_root).map_err(|error| {
            map_error(OciExtractError::Io(format!(
                "failed to create extraction root '{}': {error}",
                extract_root.display()
            )))
        })?;

        let records =
            extract_oci_image_layers(&image.object_path, &extract_root).map_err(map_error)?;
        let manifest =
            build_manifest_from_tar_records(&records, &extract_root).map_err(map_error)?;
        let config_bytes = read_oci_config_bytes(&image.object_path).map_err(map_error)?;
        let paths =
            create_oci_fs_tree_staging_dir(&staged, &manifest, &config_bytes).map_err(map_error)?;

        fs::remove_dir(&paths.fs_tree.root_dir).map_err(|error| {
            map_error(OciExtractError::Io(format!(
                "failed to replace OCI fs-tree root '{}': {error}",
                paths.fs_tree.root_dir.display()
            )))
        })?;
        fs::rename(&extract_root, &paths.fs_tree.root_dir).map_err(|error| {
            map_error(OciExtractError::Io(format!(
                "failed to move extracted root '{}' to '{}': {error}",
                extract_root.display(),
                paths.fs_tree.root_dir.display()
            )))
        })?;

        let object_hash =
            hash_oci_fs_tree_object_from_manifest(&manifest, &config_bytes).map_err(map_error)?;
        materializer
            .materialize_and_validate(&paths.fs_tree.root_dir, &manifest, &cx.temp_dir)
            .map_err(map_error)?;

        Ok(StagedBuildResult {
            staged_path: staged,
            object_hash: Some(object_hash),
        })
    }
}

fn validate_oci_layout_input(image: &BuilderInputObject) -> Result<(), OciExtractError> {
    if !image.object_path.is_dir() {
        return Err(OciExtractError::InvalidInput(format!(
            "image input must resolve to an OCI layout directory: {}",
            image.object_path.display()
        )));
    }
    for name in ["oci-layout", "index.json"] {
        let path = image.object_path.join(name);
        if !path.is_file() {
            return Err(OciExtractError::InvalidInput(format!(
                "image input is missing OCI layout file '{}'",
                path.display()
            )));
        }
    }
    let blobs = image.object_path.join("blobs").join("sha256");
    if !blobs.is_dir() {
        return Err(OciExtractError::InvalidInput(format!(
            "image input is missing OCI blobs directory '{}'",
            blobs.display()
        )));
    }
    oci::read_oci_manifest(&image.object_path)
        .map_err(|error| OciExtractError::InvalidInput(error.to_string()))?;
    Ok(())
}

trait OwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        manifest: &FsTreeManifest,
        workspace: &Path,
    ) -> Result<(), OciExtractError>;
}

struct RuntimeOwnershipMaterializer;

impl OwnershipMaterializer for RuntimeOwnershipMaterializer {
    fn materialize_and_validate(
        &self,
        root_dir: &Path,
        manifest: &FsTreeManifest,
        workspace: &Path,
    ) -> Result<(), OciExtractError> {
        let idmap = mbuild_runtime::cached_host_idmap()
            .map_err(|error| OciExtractError::Runtime(error.to_string()))?;
        mbuild_runtime::apply_ownership_batch(root_dir, manifest, &idmap, workspace)
            .map_err(|error| OciExtractError::Runtime(error.to_string()))
    }
}

fn hash_oci_fs_tree_object_from_manifest(
    manifest: &FsTreeManifest,
    config_bytes: &[u8],
) -> Result<ObjectHash, OciExtractError> {
    hash_fs_tree_object_from_manifest_with_extra_files(
        manifest,
        &[(OCI_CONFIG_FILE_NAME.as_bytes(), config_bytes)],
    )
    .map_err(|error| OciExtractError::Object(error.to_string()))
}

#[derive(Debug)]
pub enum OciExtractError {
    InvalidInput(String),
    InvalidLayer(String),
    Io(String),
    Manifest(String),
    Object(String),
    Runtime(String),
}

impl fmt::Display for OciExtractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message)
            | Self::InvalidLayer(message)
            | Self::Io(message)
            | Self::Manifest(message)
            | Self::Object(message)
            | Self::Runtime(message) => formatter.write_str(message),
        }
    }
}

impl From<FsTreeObjectError> for OciExtractError {
    fn from(error: FsTreeObjectError) -> Self {
        Self::Object(error.to_string())
    }
}

fn map_error(error: OciExtractError) -> BuilderError {
    match error {
        OciExtractError::InvalidInput(message) | OciExtractError::Manifest(message) => {
            BuilderError::InvalidRecipe(message)
        }
        OciExtractError::InvalidLayer(message)
        | OciExtractError::Io(message)
        | OciExtractError::Object(message)
        | OciExtractError::Runtime(message) => BuilderError::ExecutionFailed(message),
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedOciFsTreeObject {
    pub(crate) fs_tree: ValidatedFsTreeObject,
    pub(crate) oci_config: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciFsTreeObjectPaths {
    pub fs_tree: FsTreeObjectPaths,
    pub oci_config_path: PathBuf,
}

#[cfg(test)]
pub(crate) fn validate_oci_fs_tree_object(
    object_dir: &Path,
    owner_map: &impl FsTreeOwnerMap,
) -> Result<ValidatedOciFsTreeObject, OciExtractError> {
    let config_path = object_dir.join(OCI_CONFIG_FILE_NAME);
    let metadata = fs::symlink_metadata(&config_path).map_err(|error| {
        OciExtractError::Object(format!(
            "failed to inspect OCI config '{}': {error}",
            config_path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(OciExtractError::Object(format!(
            "OCI fs-tree config '{}' must be a regular file",
            config_path.display()
        )));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 != 0 {
        return Err(OciExtractError::Object(format!(
            "OCI fs-tree config '{}' must not be executable",
            config_path.display()
        )));
    }

    let config_bytes = fs::read(&config_path).map_err(|error| {
        OciExtractError::Object(format!(
            "failed to read OCI config '{}': {error}",
            config_path.display()
        ))
    })?;
    let oci_config = serde_json::from_slice(&config_bytes).map_err(|error| {
        OciExtractError::Object(format!(
            "failed to parse OCI config '{}': {error}",
            config_path.display()
        ))
    })?;

    let fs_tree = validate_fs_tree_object(object_dir, owner_map)?;
    Ok(ValidatedOciFsTreeObject {
        fs_tree,
        oci_config,
    })
}

pub fn create_oci_fs_tree_staging_dir(
    object_dir: &Path,
    manifest: &FsTreeManifest,
    oci_config_bytes: &[u8],
) -> Result<OciFsTreeObjectPaths, OciExtractError> {
    serde_json::from_slice::<Value>(oci_config_bytes).map_err(|error| {
        OciExtractError::Object(format!("failed to parse OCI config bytes: {error}"))
    })?;
    let fs_tree = create_fs_tree_staging_dir(object_dir, manifest)?;
    let oci_config_path = object_dir.join(OCI_CONFIG_FILE_NAME);
    fs::write(&oci_config_path, oci_config_bytes).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to write OCI config '{}': {error}",
            oci_config_path.display()
        ))
    })?;
    Ok(OciFsTreeObjectPaths {
        fs_tree,
        oci_config_path,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarEntryRecord {
    path: String,
    kind: TarRecordKind,
    uid: u32,
    gid: u32,
    mode: Option<u32>,
    symlink_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TarRecordKind {
    File,
    Directory,
    Symlink,
}

pub fn extract_oci_image_layers(
    oci_layout_dir: &Path,
    target_root: &Path,
) -> Result<Vec<TarEntryRecord>, OciExtractError> {
    let manifest = oci::read_oci_manifest(oci_layout_dir)
        .map_err(|error| OciExtractError::InvalidInput(error.to_string()))?;
    let mut records = BTreeMap::<String, TarEntryRecord>::new();

    for layer in &manifest.layers {
        extract_layer(oci_layout_dir, target_root, layer, &mut records)?;
    }

    Ok(records.into_values().collect())
}

fn extract_layer(
    oci_layout_dir: &Path,
    target_root: &Path,
    layer: &OciDescriptor,
    records: &mut BTreeMap<String, TarEntryRecord>,
) -> Result<(), OciExtractError> {
    let blob_path = oci::blob_path(oci_layout_dir, &layer.digest);
    let blob = File::open(&blob_path).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to open OCI layer blob '{}': {error}",
            blob_path.display()
        ))
    })?;

    match layer.media_type.as_str() {
        oci::MEDIA_TYPE_OCI_LAYER | MEDIA_TYPE_DOCKER_LAYER_GZIP => {
            let decoder = GzDecoder::new(blob);
            extract_tar_stream(decoder, target_root, records)
        }
        MEDIA_TYPE_OCI_LAYER_TAR => extract_tar_stream(blob, target_root, records),
        media_type => Err(OciExtractError::InvalidLayer(format!(
            "unsupported OCI layer media type '{media_type}' for {}",
            layer.digest
        ))),
    }
}

fn extract_tar_stream(
    reader: impl Read,
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
) -> Result<(), OciExtractError> {
    let mut archive = Archive::new(reader);
    archive.set_preserve_permissions(false);
    archive.set_preserve_ownerships(false);
    archive.set_preserve_mtime(false);

    let entries = archive
        .entries()
        .map_err(|error| OciExtractError::InvalidLayer(format!("failed to read tar: {error}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| {
            OciExtractError::InvalidLayer(format!("failed to read tar entry: {error}"))
        })?;
        let Some(path) = sanitize_tar_path_bytes(&entry.path_bytes(), "tar entry path")? else {
            continue;
        };

        if handle_whiteout(target_root, records, &path)? {
            continue;
        }

        warn_unsupported_xattrs(&mut entry)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_file() || entry_type.is_contiguous() {
            extract_file_entry(target_root, records, &path, &mut entry)?;
        } else if entry_type.is_dir() {
            extract_directory_entry(target_root, records, &path, &entry)?;
        } else if entry_type.is_symlink() {
            extract_symlink_entry(target_root, records, &path, &entry)?;
        } else if entry_type.is_hard_link() {
            extract_hardlink_entry(target_root, records, &path, &entry)?;
        } else if is_special_entry_type(entry_type) {
            warn!("skipping unsupported OCI layer special file '{}'", path);
        } else {
            warn!("skipping unsupported OCI layer entry '{}'", path);
        }
    }

    Ok(())
}

fn warn_unsupported_xattrs(entry: &mut tar::Entry<'_, impl Read>) -> Result<(), OciExtractError> {
    let path = String::from_utf8_lossy(entry.path_bytes().as_ref()).to_string();
    let Some(extensions) = entry.pax_extensions().map_err(|error| {
        OciExtractError::InvalidLayer(format!(
            "failed to read pax extensions for '{path}': {error}"
        ))
    })?
    else {
        return Ok(());
    };

    for extension in extensions {
        let extension = extension.map_err(|error| {
            OciExtractError::InvalidLayer(format!(
                "failed to parse pax extension for '{path}': {error}"
            ))
        })?;
        let Ok(key) = extension.key() else {
            continue;
        };
        if key.starts_with("SCHILY.xattr.")
            || key.starts_with("LIBARCHIVE.xattr.")
            || key == "security.capability"
        {
            warn!(
                "skipping unsupported OCI layer xattr '{}' on '{}'",
                key, path
            );
        }
    }
    Ok(())
}

fn extract_file_entry(
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
    path: &str,
    entry: &mut tar::Entry<'_, impl Read>,
) -> Result<(), OciExtractError> {
    ensure_parent_directories(target_root, records, path)?;
    remove_existing_path(target_root, records, path)?;

    let dst = target_root.join(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dst)
        .map_err(|error| {
            OciExtractError::Io(format!(
                "failed to create extracted file '{}': {error}",
                dst.display()
            ))
        })?;
    io::copy(entry, &mut file).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to write extracted file '{}': {error}",
            dst.display()
        ))
    })?;

    records.insert(
        path.to_string(),
        record_from_entry(path, TarRecordKind::File, entry, None)?,
    );
    Ok(())
}

fn extract_directory_entry(
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
    path: &str,
    entry: &tar::Entry<'_, impl Read>,
) -> Result<(), OciExtractError> {
    ensure_parent_directories(target_root, records, path)?;
    match records.get(path).map(|record| record.kind) {
        Some(TarRecordKind::Directory) => {}
        Some(_) => remove_existing_path(target_root, records, path)?,
        None => {}
    }

    let dst = target_root.join(path);
    fs::create_dir_all(&dst).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to create extracted directory '{}': {error}",
            dst.display()
        ))
    })?;
    records.insert(
        path.to_string(),
        record_from_entry(path, TarRecordKind::Directory, entry, None)?,
    );
    Ok(())
}

fn extract_symlink_entry(
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
    path: &str,
    entry: &tar::Entry<'_, impl Read>,
) -> Result<(), OciExtractError> {
    ensure_parent_directories(target_root, records, path)?;
    remove_existing_path(target_root, records, path)?;

    let target = entry.link_name().map_err(|error| {
        OciExtractError::InvalidLayer(format!(
            "failed to read symlink target for '{path}': {error}"
        ))
    })?;
    let target = target.ok_or_else(|| {
        OciExtractError::InvalidLayer(format!("symlink entry '{path}' has no target"))
    })?;
    let target_string = target.to_str().ok_or_else(|| {
        OciExtractError::InvalidLayer(format!("symlink target for '{path}' is not UTF-8"))
    })?;
    let dst = target_root.join(path);
    symlink(&target, &dst).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to create extracted symlink '{}': {error}",
            dst.display()
        ))
    })?;
    records.insert(
        path.to_string(),
        record_from_entry(
            path,
            TarRecordKind::Symlink,
            entry,
            Some(target_string.to_string()),
        )?,
    );
    Ok(())
}

fn extract_hardlink_entry(
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
    path: &str,
    entry: &tar::Entry<'_, impl Read>,
) -> Result<(), OciExtractError> {
    let link_name = entry.link_name_bytes().ok_or_else(|| {
        OciExtractError::InvalidLayer(format!("hardlink entry '{path}' has no target"))
    })?;
    let target = sanitize_tar_path_bytes(&link_name, "hardlink target")?.ok_or_else(|| {
        OciExtractError::InvalidLayer(format!("hardlink entry '{path}' targets archive root"))
    })?;
    let target_record = records.get(&target).cloned().ok_or_else(|| {
        OciExtractError::InvalidLayer(format!(
            "hardlink entry '{path}' references missing target '{target}'"
        ))
    })?;
    if target_record.kind != TarRecordKind::File {
        return Err(OciExtractError::InvalidLayer(format!(
            "hardlink entry '{path}' references non-file target '{target}'"
        )));
    }

    let mut link_record = record_from_entry(path, TarRecordKind::File, entry, None)?;
    if link_record.mode == Some(0) {
        link_record.mode = target_record.mode;
    }
    if link_record.uid != target_record.uid
        || link_record.gid != target_record.gid
        || link_record.mode != target_record.mode
    {
        return Err(OciExtractError::InvalidLayer(format!(
            "hardlink entry '{path}' has ownership or mode different from target '{target}'"
        )));
    }

    ensure_parent_directories(target_root, records, path)?;
    remove_existing_path(target_root, records, path)?;
    let src = target_root.join(&target);
    let dst = target_root.join(path);
    fs::hard_link(&src, &dst).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to create extracted hardlink '{}' -> '{}': {error}",
            dst.display(),
            src.display()
        ))
    })?;
    records.insert(path.to_string(), link_record);
    Ok(())
}

fn record_from_entry(
    path: &str,
    kind: TarRecordKind,
    entry: &tar::Entry<'_, impl Read>,
    symlink_target: Option<String>,
) -> Result<TarEntryRecord, OciExtractError> {
    let uid = checked_u32(
        entry.header().uid().map_err(|error| {
            OciExtractError::InvalidLayer(format!("failed to read uid for '{path}': {error}"))
        })?,
        "uid",
        path,
    )?;
    let gid = checked_u32(
        entry.header().gid().map_err(|error| {
            OciExtractError::InvalidLayer(format!("failed to read gid for '{path}': {error}"))
        })?,
        "gid",
        path,
    )?;
    let mode = if kind == TarRecordKind::Symlink {
        None
    } else {
        Some(
            entry.header().mode().map_err(|error| {
                OciExtractError::InvalidLayer(format!("failed to read mode for '{path}': {error}"))
            })? & 0o7777,
        )
    };
    Ok(TarEntryRecord {
        path: path.to_string(),
        kind,
        uid,
        gid,
        mode,
        symlink_target,
    })
}

fn checked_u32(value: u64, field: &str, path: &str) -> Result<u32, OciExtractError> {
    u32::try_from(value).map_err(|_| {
        OciExtractError::InvalidLayer(format!(
            "{field} {value} for tar entry '{path}' exceeds u32"
        ))
    })
}

fn is_special_entry_type(entry_type: EntryType) -> bool {
    entry_type.is_character_special()
        || entry_type.is_block_special()
        || entry_type.is_fifo()
        || entry_type.is_gnu_sparse()
}

fn handle_whiteout(
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
    path: &str,
) -> Result<bool, OciExtractError> {
    let Some((parent, name)) = split_parent_name(path) else {
        return Ok(false);
    };
    if name == ".wh..wh..opq" {
        let dir = target_root.join(parent);
        match fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        OciExtractError::Io(format!(
                            "failed to read opaque whiteout directory '{}': {error}",
                            dir.display()
                        ))
                    })?;
                    remove_fs_path(&entry.path())?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(OciExtractError::Io(format!(
                    "failed to read opaque whiteout directory '{}': {error}",
                    dir.display()
                )));
            }
        }
        remove_record_children(records, parent);
        return Ok(true);
    }

    if let Some(rest) = name.strip_prefix(".wh.") {
        if rest.is_empty() {
            return Err(OciExtractError::InvalidLayer(format!(
                "invalid empty whiteout entry '{path}'"
            )));
        }
        let victim = join_rel(parent, rest);
        let victim_path = target_root.join(&victim);
        match fs::symlink_metadata(&victim_path) {
            Ok(_) => remove_fs_path(&victim_path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(OciExtractError::Io(format!(
                    "failed to inspect whiteout target '{}': {error}",
                    victim_path.display()
                )));
            }
        }
        remove_record_tree(records, &victim);
        return Ok(true);
    }

    Ok(false)
}

fn remove_existing_path(
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
    path: &str,
) -> Result<(), OciExtractError> {
    let fs_path = target_root.join(path);
    match fs::symlink_metadata(&fs_path) {
        Ok(_) => remove_fs_path(&fs_path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(OciExtractError::Io(format!(
                "failed to inspect existing extracted path '{}': {error}",
                fs_path.display()
            )));
        }
    }
    remove_record_tree(records, path);
    Ok(())
}

fn remove_fs_path(path: &Path) -> Result<(), OciExtractError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to inspect path '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).map_err(|error| {
            OciExtractError::Io(format!(
                "failed to remove directory '{}': {error}",
                path.display()
            ))
        })
    } else {
        fs::remove_file(path).map_err(|error| {
            OciExtractError::Io(format!(
                "failed to remove file '{}': {error}",
                path.display()
            ))
        })
    }
}

fn remove_record_tree(records: &mut BTreeMap<String, TarEntryRecord>, path: &str) {
    let prefix = format!("{path}/");
    records.retain(|candidate, _| candidate != path && !candidate.starts_with(&prefix));
}

fn remove_record_children(records: &mut BTreeMap<String, TarEntryRecord>, path: &str) {
    if path.is_empty() {
        records.clear();
        return;
    }
    let prefix = format!("{path}/");
    records.retain(|candidate, _| {
        if candidate == path {
            true
        } else {
            !candidate.starts_with(&prefix)
        }
    });
}

fn ensure_parent_directories(
    target_root: &Path,
    records: &mut BTreeMap<String, TarEntryRecord>,
    path: &str,
) -> Result<(), OciExtractError> {
    let mut current = String::new();
    for component in path
        .split('/')
        .take(path.split('/').count().saturating_sub(1))
    {
        current = join_rel(&current, component);
        match records.get(&current) {
            Some(record) if record.kind == TarRecordKind::Directory => {}
            Some(_) => {
                return Err(OciExtractError::InvalidLayer(format!(
                    "tar entry '{path}' has non-directory parent '{current}'"
                )));
            }
            None => {
                let dir = target_root.join(&current);
                fs::create_dir_all(&dir).map_err(|error| {
                    OciExtractError::Io(format!(
                        "failed to create implicit directory '{}': {error}",
                        dir.display()
                    ))
                })?;
                records.insert(
                    current.clone(),
                    TarEntryRecord {
                        path: current.clone(),
                        kind: TarRecordKind::Directory,
                        uid: 0,
                        gid: 0,
                        mode: Some(0o755),
                        symlink_target: None,
                    },
                );
            }
        }
    }
    Ok(())
}

pub fn build_manifest_from_tar_records(
    records: &[TarEntryRecord],
    root_dir: &Path,
) -> Result<FsTreeManifest, OciExtractError> {
    let mut by_path = BTreeMap::<String, FsTreeEntry>::new();
    by_path.insert(String::new(), FsTreeEntry::directory("", 0, 0, 0o755));

    for record in records {
        add_implicit_manifest_parents(&record.path, &mut by_path);
        let entry = match record.kind {
            TarRecordKind::File => {
                let mode = record.mode.expect("file record has mode");
                let bytes = fs::read(root_dir.join(&record.path)).map_err(|error| {
                    OciExtractError::Io(format!(
                        "failed to read extracted file '{}': {error}",
                        root_dir.join(&record.path).display()
                    ))
                })?;
                FsTreeEntry::file_with_hash(
                    &record.path,
                    record.uid,
                    record.gid,
                    mode,
                    hash_file_bytes(mode & 0o111 != 0, &bytes),
                )
            }
            TarRecordKind::Directory => FsTreeEntry::directory(
                &record.path,
                record.uid,
                record.gid,
                record.mode.expect("directory record has mode"),
            ),
            TarRecordKind::Symlink => {
                let target = record
                    .symlink_target
                    .as_deref()
                    .expect("symlink record has target");
                FsTreeEntry::symlink_with_hash(
                    &record.path,
                    record.uid,
                    record.gid,
                    target,
                    hash_symlink_node(target.as_bytes()),
                )
            }
        };
        by_path.insert(record.path.clone(), entry);
    }

    FsTreeManifest::from_entries(by_path.into_values().collect())
        .map_err(|error| OciExtractError::Manifest(error.to_string()))
}

fn add_implicit_manifest_parents(path: &str, by_path: &mut BTreeMap<String, FsTreeEntry>) {
    let mut current = String::new();
    for component in path
        .split('/')
        .take(path.split('/').count().saturating_sub(1))
    {
        current = join_rel(&current, component);
        by_path
            .entry(current.clone())
            .or_insert_with(|| FsTreeEntry::directory(&current, 0, 0, 0o755));
    }
}

fn read_oci_config_bytes(oci_layout_dir: &Path) -> Result<Vec<u8>, OciExtractError> {
    let manifest = oci::read_oci_manifest(oci_layout_dir)
        .map_err(|error| OciExtractError::InvalidInput(error.to_string()))?;
    read_config_bytes_for_manifest(oci_layout_dir, &manifest)
}

fn read_config_bytes_for_manifest(
    oci_layout_dir: &Path,
    manifest: &OciManifest,
) -> Result<Vec<u8>, OciExtractError> {
    let path = oci::blob_path(oci_layout_dir, &manifest.config.digest);
    let bytes = fs::read(&path).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to read OCI config blob '{}': {error}",
            path.display()
        ))
    })?;
    serde_json::from_slice::<Value>(&bytes).map_err(|error| {
        OciExtractError::InvalidInput(format!(
            "failed to parse OCI config blob '{}': {error}",
            path.display()
        ))
    })?;
    Ok(bytes)
}

fn sanitize_tar_path_bytes(bytes: &[u8], label: &str) -> Result<Option<String>, OciExtractError> {
    let raw = std::str::from_utf8(bytes)
        .map_err(|error| OciExtractError::InvalidLayer(format!("{label} is not UTF-8: {error}")))?;
    let raw = raw.replace('\\', "/");
    if raw.starts_with('/') {
        return Err(OciExtractError::InvalidLayer(format!(
            "{label} '{raw}' must be relative"
        )));
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(None);
    }
    if trimmed.contains("//") {
        return Err(OciExtractError::InvalidLayer(format!(
            "{label} '{raw}' contains an empty path component"
        )));
    }
    let mut components = Vec::new();
    for component in trimmed.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(OciExtractError::InvalidLayer(format!(
                "{label} '{raw}' contains unsafe component '{component}'"
            )));
        }
        components.push(component);
    }
    Ok(Some(components.join("/")))
}

fn split_parent_name(path: &str) -> Option<(&str, &str)> {
    path.rsplit_once('/')
        .map_or(Some(("", path)), |(parent, name)| Some((parent, name)))
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use mbuild_core::{Builder, BuilderInputObject};
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    #[test]
    fn builder_spec_is_registered_shape() {
        assert_eq!(OCI_EXTRACT_SPEC.tag, "OciExtract");
        assert_eq!(OCI_EXTRACT_SPEC.required_inputs, &["image"]);
        assert!(!OCI_EXTRACT_SPEC.allow_extra_inputs);
    }

    #[test]
    fn oci_fs_tree_validator_requires_config_and_accepts_extra_sidecars() {
        let temp = tempdir().unwrap();
        let object = temp.path().join("object");
        let manifest =
            FsTreeManifest::from_entries(vec![FsTreeEntry::directory("", 0, 0, 0o755)]).unwrap();
        let paths =
            create_oci_fs_tree_staging_dir(&object, &manifest, br#"{"config":{}}"#).unwrap();
        fs::write(object.join("extra.txt"), b"extra").unwrap();

        validate_oci_fs_tree_object(&object, &CurrentOwnerMap).unwrap();

        fs::remove_file(paths.oci_config_path).unwrap();
        assert!(validate_oci_fs_tree_object(&object, &CurrentOwnerMap).is_err());
    }

    #[test]
    fn extracts_gzip_layer_with_file_dir_and_symlink() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| {
            append_dir(builder, "bin", 0, 0, 0o755);
            append_file(builder, "bin/tool", b"hello\n", 1, 2, 0o755);
            append_symlink(builder, "tool-link", "bin/tool", 3, 4);
        });
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let records = extract_oci_image_layers(&oci, &root).unwrap();
        let manifest = build_manifest_from_tar_records(&records, &root).unwrap();

        assert_eq!(fs::read(root.join("bin/tool")).unwrap(), b"hello\n");
        assert_eq!(
            fs::read_link(root.join("tool-link")).unwrap(),
            Path::new("bin/tool")
        );
        assert!(manifest.entries().iter().any(|entry| {
            matches!(entry, FsTreeEntry::File { path, uid: 1, gid: 2, mode: 0o755, .. } if path == "bin/tool")
        }));
        assert!(manifest.entries().iter().any(|entry| {
            matches!(
                entry,
                FsTreeEntry::Symlink {
                    path,
                    uid: 3,
                    gid: 4,
                    target,
                    ..
                } if path == "tool-link" && target == "bin/tool"
            )
        }));
    }

    #[test]
    fn extracts_uncompressed_tar_layer() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| append_file(builder, "file", b"plain", 0, 0, 0o644));
        let oci = create_oci_layout(temp.path(), vec![(MEDIA_TYPE_OCI_LAYER_TAR, tar)]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        extract_oci_image_layers(&oci, &root).unwrap();

        assert_eq!(fs::read(root.join("file")).unwrap(), b"plain");
    }

    #[test]
    fn rejects_path_traversal_entries() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| append_raw_file(builder, "../escape", b"bad", 0, 0, 0o644));
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let error = extract_oci_image_layers(&oci, &root).unwrap_err();

        assert!(error.to_string().contains("unsafe component"));
    }

    #[test]
    fn applies_whiteout_file_across_layers() {
        let temp = tempdir().unwrap();
        let lower = make_tar(|builder| append_file(builder, "etc/old", b"old", 0, 0, 0o644));
        let upper = make_tar(|builder| append_file(builder, "etc/.wh.old", b"", 0, 0, 0o000));
        let oci = create_oci_layout(
            temp.path(),
            vec![
                (oci::MEDIA_TYPE_OCI_LAYER, gzip(&lower)),
                (oci::MEDIA_TYPE_OCI_LAYER, gzip(&upper)),
            ],
        );
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let records = extract_oci_image_layers(&oci, &root).unwrap();

        assert!(!root.join("etc/old").exists());
        assert!(!records.iter().any(|record| record.path == "etc/old"));
    }

    #[test]
    fn applies_opaque_whiteout_across_layers() {
        let temp = tempdir().unwrap();
        let lower = make_tar(|builder| {
            append_file(builder, "etc/a", b"a", 0, 0, 0o644);
            append_file(builder, "etc/b", b"b", 0, 0, 0o644);
        });
        let upper = make_tar(|builder| {
            append_file(builder, "etc/.wh..wh..opq", b"", 0, 0, 0o000);
            append_file(builder, "etc/c", b"c", 0, 0, 0o644);
        });
        let oci = create_oci_layout(
            temp.path(),
            vec![
                (oci::MEDIA_TYPE_OCI_LAYER, gzip(&lower)),
                (oci::MEDIA_TYPE_OCI_LAYER, gzip(&upper)),
            ],
        );
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let records = extract_oci_image_layers(&oci, &root).unwrap();

        assert!(!root.join("etc/a").exists());
        assert!(!root.join("etc/b").exists());
        assert_eq!(fs::read(root.join("etc/c")).unwrap(), b"c");
        assert!(!records.iter().any(|record| record.path == "etc/a"));
        assert!(records.iter().any(|record| record.path == "etc/c"));
    }

    #[test]
    fn applies_root_opaque_whiteout_across_layers() {
        let temp = tempdir().unwrap();
        let lower = make_tar(|builder| {
            append_file(builder, "a", b"a", 0, 0, 0o644);
            append_file(builder, "dir/b", b"b", 0, 0, 0o644);
        });
        let upper = make_tar(|builder| {
            append_file(builder, ".wh..wh..opq", b"", 0, 0, 0o000);
            append_file(builder, "c", b"c", 0, 0, 0o644);
        });
        let oci = create_oci_layout(
            temp.path(),
            vec![
                (oci::MEDIA_TYPE_OCI_LAYER, gzip(&lower)),
                (oci::MEDIA_TYPE_OCI_LAYER, gzip(&upper)),
            ],
        );
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let records = extract_oci_image_layers(&oci, &root).unwrap();

        assert!(!root.join("a").exists());
        assert!(!root.join("dir").exists());
        assert_eq!(fs::read(root.join("c")).unwrap(), b"c");
        assert_eq!(
            records
                .iter()
                .map(|record| record.path.as_str())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
    }

    #[test]
    fn manifest_builder_adds_implicit_parent_directories() {
        let records = vec![TarEntryRecord {
            path: "usr/bin/tool".to_string(),
            kind: TarRecordKind::File,
            uid: 0,
            gid: 0,
            mode: Some(0o755),
            symlink_target: None,
        }];
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        fs::write(root.path().join("usr/bin/tool"), b"tool").unwrap();

        let manifest = build_manifest_from_tar_records(&records, root.path()).unwrap();

        assert!(manifest.entries().iter().any(|entry| {
            matches!(entry, FsTreeEntry::Directory { path, uid: 0, gid: 0, mode: 0o755 } if path == "usr")
        }));
        assert!(manifest.entries().iter().any(|entry| {
            matches!(entry, FsTreeEntry::Directory { path, uid: 0, gid: 0, mode: 0o755 } if path == "usr/bin")
        }));
    }

    #[test]
    fn preserves_compatible_tar_hardlinks() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| {
            append_file(builder, "bin/tool", b"tool", 0, 0, 0o755);
            append_hardlink(builder, "bin/tool2", "bin/tool", 0, 0, 0o755);
        });
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let records = extract_oci_image_layers(&oci, &root).unwrap();

        assert!(records.iter().any(|record| record.path == "bin/tool2"));
        let left = fs::metadata(root.join("bin/tool")).unwrap();
        let right = fs::metadata(root.join("bin/tool2")).unwrap();
        assert_eq!(left.ino(), right.ino());
    }

    #[test]
    fn rejects_hardlinks_with_incompatible_attrs() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| {
            append_file(builder, "bin/tool", b"tool", 0, 0, 0o755);
            append_hardlink(builder, "bin/tool2", "bin/tool", 0, 0, 0o644);
        });
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let error = extract_oci_image_layers(&oci, &root).unwrap_err();

        assert!(error.to_string().contains("different from target"));
    }

    #[test]
    fn builder_materializes_with_fake_owner_materializer() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let tar = make_tar(|builder| append_file(builder, "bin/tool", b"tool", 0, 0, 0o755));
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "image",
            BuilderInputObject {
                object_hash: fsobj_hash::hash_path(&oci).unwrap(),
                object_path: oci,
            },
        );

        let result = OciExtractBuilder
            .build_with_materializer(
                OciExtractConfig {},
                inputs,
                &mut cx,
                &CurrentOwnerMaterializer,
            )
            .unwrap();
        assert!(result.staged_path.join("manifest.jsonl").is_file());
        assert!(result.staged_path.join("root/bin/tool").is_file());
        assert!(result.staged_path.join("oci-config.json").is_file());
        assert_eq!(
            result.object_hash,
            Some(expected_oci_object_hash(&result.staged_path))
        );
    }

    #[test]
    fn builder_does_not_host_validate_after_precomputing_hash() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let tar = make_tar(|builder| append_dir(builder, "private", 0, 0, 0o000));
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "image",
            BuilderInputObject {
                object_hash: fsobj_hash::hash_path(&oci).unwrap(),
                object_path: oci,
            },
        );

        let result = OciExtractBuilder
            .build_with_materializer(
                OciExtractConfig {},
                inputs,
                &mut cx,
                &UnreadableOwnerMaterializer,
            )
            .unwrap();

        assert_eq!(
            result.object_hash,
            Some(expected_oci_object_hash(&result.staged_path))
        );
        assert!(result.staged_path.join("root/private").is_dir());
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let mut inputs = BuilderInputs::empty();
        let image = create_oci_layout(temp.path(), vec![]);
        inputs.insert(
            "image",
            BuilderInputObject {
                object_hash: fsobj_hash::hash_path(&image).unwrap(),
                object_path: image,
            },
        );

        let error = OciExtractBuilder
            .build_erased(serde_json::json!({ "unexpected": true }), inputs, &mut cx)
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    struct CurrentOwnerMaterializer;

    impl OwnershipMaterializer for CurrentOwnerMaterializer {
        fn materialize_and_validate(
            &self,
            root_dir: &Path,
            manifest: &FsTreeManifest,
            _: &Path,
        ) -> Result<(), OciExtractError> {
            apply_manifest_modes(root_dir, manifest);
            Ok(())
        }
    }

    struct UnreadableOwnerMaterializer;

    impl OwnershipMaterializer for UnreadableOwnerMaterializer {
        fn materialize_and_validate(
            &self,
            root_dir: &Path,
            manifest: &FsTreeManifest,
            _: &Path,
        ) -> Result<(), OciExtractError> {
            apply_manifest_modes(root_dir, manifest);
            fs::set_permissions(root_dir.join("private"), fs::Permissions::from_mode(0o000))
                .unwrap();
            Ok(())
        }
    }

    fn apply_manifest_modes(root: &Path, manifest: &FsTreeManifest) {
        for entry in manifest.entries() {
            if let FsTreeEntry::File { path, mode, .. } = entry {
                fs::set_permissions(root.join(path), fs::Permissions::from_mode(*mode)).unwrap();
            }
        }
        let mut dirs = manifest
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                FsTreeEntry::Directory { path, mode, .. } => Some((path, mode)),
                _ => None,
            })
            .collect::<Vec<_>>();
        dirs.sort_by(|(left, _), (right, _)| right.len().cmp(&left.len()));
        for (path, mode) in dirs {
            fs::set_permissions(root.join(path), fs::Permissions::from_mode(*mode)).unwrap();
        }
    }

    struct CurrentOwnerMap;

    impl FsTreeOwnerMap for CurrentOwnerMap {
        fn physical_uid(&self, logical_uid: u32) -> Result<u32, FsTreeObjectError> {
            if logical_uid == 0 {
                Ok(unsafe { libc::geteuid() })
            } else {
                Err(FsTreeObjectError::Invalid("non-current uid".to_string()))
            }
        }

        fn physical_gid(&self, logical_gid: u32) -> Result<u32, FsTreeObjectError> {
            if logical_gid == 0 {
                Ok(unsafe { libc::getegid() })
            } else {
                Err(FsTreeObjectError::Invalid("non-current gid".to_string()))
            }
        }
    }

    fn expected_oci_object_hash(object_dir: &Path) -> ObjectHash {
        let manifest = FsTreeManifest::read_canonical(&object_dir.join("manifest.jsonl")).unwrap();
        let config_bytes = fs::read(object_dir.join(OCI_CONFIG_FILE_NAME)).unwrap();
        hash_oci_fs_tree_object_from_manifest(&manifest, &config_bytes).unwrap()
    }

    fn build_context(root: &Path) -> BuildContext {
        let state_dir = root.join("builder");
        let temp_dir = root.join("tmp");
        fs::create_dir_all(&state_dir).unwrap();
        mbuild_core::fsutil::recreate_empty_dir_force(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    fn create_oci_layout(root: &Path, layers: Vec<(&str, Vec<u8>)>) -> PathBuf {
        let oci_dir = root.join(format!("oci-{}", layers.len()));
        fs::create_dir_all(oci_dir.join("blobs").join("sha256")).unwrap();
        fs::write(
            oci_dir.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let config_bytes = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},"config":{}}"#;
        let config_desc = write_test_blob(&oci_dir, config_bytes, oci::MEDIA_TYPE_OCI_CONFIG);
        let layer_descs = layers
            .into_iter()
            .map(|(media_type, bytes)| write_test_blob(&oci_dir, &bytes, media_type))
            .collect::<Vec<_>>();
        let manifest = oci::OciManifest {
            schema_version: 2,
            config: config_desc,
            layers: layer_descs,
        };
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_desc =
            write_test_blob(&oci_dir, &manifest_bytes, oci::MEDIA_TYPE_OCI_MANIFEST);
        oci::write_index(&oci_dir, manifest_desc, None).unwrap();
        oci_dir
    }

    fn write_test_blob(oci_dir: &Path, bytes: &[u8], media_type: &str) -> OciDescriptor {
        let hex = format!("{:x}", Sha256::digest(bytes));
        fs::write(oci_dir.join("blobs").join("sha256").join(&hex), bytes).unwrap();
        OciDescriptor {
            media_type: media_type.to_string(),
            digest: format!("sha256:{hex}"),
            size: bytes.len() as u64,
            platform: None,
            annotations: None,
        }
    }

    fn make_tar(fill: impl FnOnce(&mut tar::Builder<&mut Vec<u8>>)) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            fill(&mut builder);
            builder.finish().unwrap();
        }
        bytes
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn append_file(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &str,
        bytes: &[u8],
        uid: u64,
        gid: u64,
        mode: u32,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mode(mode);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(bytes))
            .unwrap();
    }

    fn append_raw_file(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &str,
        bytes: &[u8],
        uid: u64,
        gid: u64,
        mode: u32,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mode(mode);
        let path_bytes = path.as_bytes();
        header.as_mut_bytes()[0..path_bytes.len()].copy_from_slice(path_bytes);
        header.set_cksum();
        builder.append(&header, Cursor::new(bytes)).unwrap();
    }

    fn append_dir(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &str,
        uid: u64,
        gid: u64,
        mode: u32,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Directory);
        header.set_size(0);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mode(mode);
        header.set_cksum();
        builder.append_data(&mut header, path, io::empty()).unwrap();
    }

    fn append_symlink(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &str,
        target: &str,
        uid: u64,
        gid: u64,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mode(0o777);
        header.set_link_name(target).unwrap();
        header.set_cksum();
        builder.append_data(&mut header, path, io::empty()).unwrap();
    }

    fn append_hardlink(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &str,
        target: &str,
        uid: u64,
        gid: u64,
        mode: u32,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mode(mode);
        header.set_link_name(target).unwrap();
        header.set_cksum();
        builder.append_data(&mut header, path, io::empty()).unwrap();
    }
}
