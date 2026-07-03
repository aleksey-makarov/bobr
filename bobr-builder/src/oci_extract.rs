use crate::{
    BuildContext, BuilderError, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder,
};
use bobr_core::{
    BuildLogLevel,
    oci::{self, OciDescriptor, OciManifest},
};
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use bobr_store::fs_tree::FsTree;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tar::{Archive, EntryType};

const OUTPUT_MANIFEST_FILE_NAME: &str = "fs-tree-manifest.jsonl";
const EXTRACT_ROOT_DIR_NAME: &str = "oci-extract-root";
const MEDIA_TYPE_DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const MEDIA_TYPE_OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

/// Configuration for [`OciExtractBuilder`] (no options).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciExtractConfig {}

/// Extracts the layers of an OCI image (the `image` input) into a single
/// fs-tree.
#[derive(Debug)]
pub struct OciExtractBuilder;

static OCI_EXTRACT_SPEC: InputSpec = InputSpec {
    required_inputs: &["image"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for OciExtractBuilder {
    type Config = OciExtractConfig;

    fn tag(&self) -> &'static str {
        "OciExtract"
    }

    fn impl_version(&self) -> &'static str {
        "1"
    }

    fn spec(&self) -> &'static InputSpec {
        &OCI_EXTRACT_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        self.build_typed_inner(config, inputs, cx)
    }
}

impl OciExtractBuilder {
    fn build_typed_inner(
        &self,
        _config: OciExtractConfig,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        let image = inputs.required("image")?;
        validate_oci_layout_path(image).map_err(map_error)?;

        let output_manifest = cx.temp_dir.join(OUTPUT_MANIFEST_FILE_NAME);
        let extract_root = cx.temp_dir.join(EXTRACT_ROOT_DIR_NAME);
        if output_manifest.exists() || extract_root.exists() {
            return Err(map_error(OciExtractError::InvalidInput(format!(
                "OciExtract staging paths already exist under '{}'",
                cx.temp_dir.display()
            ))));
        }
        let fs_tree = cx.fs_tree();

        cx.log_event(
            BuildLogLevel::Info,
            "extract",
            format!("extracting OCI image '{}' into fs-tree", image.display()),
        );

        let output = cx
            .runtime()
            .run(
                &OciExtractFunction,
                OciExtractInput {
                    oci_layout_dir: image.clone(),
                    extract_root,
                    output_manifest: output_manifest.clone(),
                    fs_tree,
                },
            )
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;

        if !output.warnings.is_empty() {
            let raw_log_path = cx.write_raw_log(
                "oci-extract-warnings",
                &format!("{}\n", output.warnings.join("\n")),
            );
            cx.log_event_with_details(
                BuildLogLevel::Warn,
                "extract",
                format!(
                    "skipped {} unsupported OCI layer entr{}",
                    output.warnings.len(),
                    if output.warnings.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                ),
                None,
                raw_log_path,
                serde_json::Map::new(),
            );
        }

        cx.log_event(
            BuildLogLevel::Info,
            "extract",
            format!("wrote fs-tree manifest with {} entries", output.entries),
        );

        Ok(StagedBuildResult {
            staged_path: output_manifest,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OciExtractFunction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OciExtractInput {
    oci_layout_dir: PathBuf,
    extract_root: PathBuf,
    output_manifest: PathBuf,
    fs_tree: FsTree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OciExtractOutput {
    entries: usize,
    /// Human-readable warnings for entries the extraction skipped (unsupported
    /// layer entries, special files, xattrs). Returned as a bulk payload so the
    /// builder, which owns the logger, can write a raw log and emit one event.
    warnings: Vec<String>,
}

impl RuntimeFunction for OciExtractFunction {
    type Input = OciExtractInput;
    type Output = OciExtractOutput;

    fn name(&self) -> &'static str {
        "oci-extract"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        extract_oci_image_to_fs_tree(input).map_err(|error| RuntimeError::new(error.to_string()))
    }
}

fn extract_oci_image_to_fs_tree(
    input: OciExtractInput,
) -> Result<OciExtractOutput, OciExtractError> {
    validate_oci_layout_path(&input.oci_layout_dir)?;
    if input.extract_root.exists() || input.output_manifest.exists() {
        return Err(OciExtractError::InvalidInput(format!(
            "OciExtract runtime paths already exist under '{}' or '{}'",
            input.extract_root.display(),
            input.output_manifest.display()
        )));
    }

    fs::create_dir(&input.extract_root).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to create extraction root '{}': {error}",
            input.extract_root.display()
        ))
    })?;

    let output = extract_oci_image_to_fs_tree_inner(&input);
    let cleanup = remove_extraction_root(&input.extract_root);
    match (output, cleanup) {
        (Ok(output), Ok(())) => Ok(output),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(OciExtractError::Io(format!(
            "{error}; additionally failed to clean OCI extraction root '{}': {cleanup_error}",
            input.extract_root.display()
        ))),
    }
}

fn extract_oci_image_to_fs_tree_inner(
    input: &OciExtractInput,
) -> Result<OciExtractOutput, OciExtractError> {
    let mut warnings = Vec::new();
    let records =
        extract_oci_image_layers(&input.oci_layout_dir, &input.extract_root, &mut warnings)?;
    read_oci_config_bytes(&input.oci_layout_dir)?;
    apply_extracted_metadata(&input.extract_root, &records)?;
    let manifest = input.fs_tree.scan(&input.extract_root).map_err(|error| {
        OciExtractError::Object(format!(
            "failed to scan extracted OCI filesystem '{}': {error}",
            input.extract_root.display()
        ))
    })?;
    let entries = manifest.entries().len();
    manifest
        .write_canonical(&input.output_manifest)
        .map_err(|error| OciExtractError::Object(error.to_string()))?;
    Ok(OciExtractOutput { entries, warnings })
}

fn remove_extraction_root(path: &Path) -> Result<(), OciExtractError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OciExtractError::Io(format!(
            "failed to remove OCI extraction root '{}': {error}",
            path.display()
        ))),
    }
}

fn validate_oci_layout_path(path: &Path) -> Result<(), OciExtractError> {
    if !path.is_dir() {
        return Err(OciExtractError::InvalidInput(format!(
            "image input must resolve to an OCI layout directory: {}",
            path.display()
        )));
    }
    for name in ["oci-layout", "index.json"] {
        let child = path.join(name);
        if !child.is_file() {
            return Err(OciExtractError::InvalidInput(format!(
                "image input is missing OCI layout file '{}'",
                child.display()
            )));
        }
    }
    let blobs = path.join("blobs").join("sha256");
    if !blobs.is_dir() {
        return Err(OciExtractError::InvalidInput(format!(
            "image input is missing OCI blobs directory '{}'",
            blobs.display()
        )));
    }
    oci::read_oci_manifest(path)
        .map_err(|error| OciExtractError::InvalidInput(error.to_string()))?;
    Ok(())
}

#[derive(Debug)]
enum OciExtractError {
    InvalidInput(String),
    InvalidLayer(String),
    Io(String),
    Object(String),
}

impl fmt::Display for OciExtractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message)
            | Self::InvalidLayer(message)
            | Self::Io(message)
            | Self::Object(message) => formatter.write_str(message),
        }
    }
}

fn map_error(error: OciExtractError) -> BuilderError {
    match error {
        OciExtractError::InvalidInput(message) => BuilderError::InvalidRecipe(message),
        OciExtractError::InvalidLayer(message)
        | OciExtractError::Io(message)
        | OciExtractError::Object(message) => BuilderError::ExecutionFailed(message),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TarEntryRecord {
    path: String,
    kind: TarRecordKind,
    uid: u32,
    gid: u32,
    mode: u32,
    symlink_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TarRecordKind {
    File,
    Directory,
    Symlink,
}

fn extract_oci_image_layers(
    oci_layout_dir: &Path,
    target_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<TarEntryRecord>, OciExtractError> {
    let manifest = oci::read_oci_manifest(oci_layout_dir)
        .map_err(|error| OciExtractError::InvalidInput(error.to_string()))?;
    let mut records = BTreeMap::<String, TarEntryRecord>::new();

    for layer in &manifest.layers {
        extract_layer(oci_layout_dir, target_root, layer, &mut records, warnings)?;
    }

    Ok(records.into_values().collect())
}

fn extract_layer(
    oci_layout_dir: &Path,
    target_root: &Path,
    layer: &OciDescriptor,
    records: &mut BTreeMap<String, TarEntryRecord>,
    warnings: &mut Vec<String>,
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
            extract_tar_stream(decoder, target_root, records, warnings)
        }
        MEDIA_TYPE_OCI_LAYER_TAR => extract_tar_stream(blob, target_root, records, warnings),
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
    warnings: &mut Vec<String>,
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

        warn_unsupported_xattrs(&mut entry, warnings)?;
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
            warnings.push(format!(
                "skipping unsupported OCI layer special file '{path}'"
            ));
        } else {
            warnings.push(format!("skipping unsupported OCI layer entry '{path}'"));
        }
    }

    Ok(())
}

fn warn_unsupported_xattrs(
    entry: &mut tar::Entry<'_, impl Read>,
    warnings: &mut Vec<String>,
) -> Result<(), OciExtractError> {
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
            warnings.push(format!(
                "skipping unsupported OCI layer xattr '{key}' on '{path}'"
            ));
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
    if link_record.mode == 0 {
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
    let mode = entry.header().mode().map_err(|error| {
        OciExtractError::InvalidLayer(format!("failed to read mode for '{path}': {error}"))
    })? & 0o7777;
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
                        mode: 0o755,
                        symlink_target: None,
                    },
                );
            }
        }
    }
    Ok(())
}

fn apply_extracted_metadata(
    root: &Path,
    records: &[TarEntryRecord],
) -> Result<(), OciExtractError> {
    let mut directories = Vec::new();
    for record in records {
        let path = root.join(&record.path);
        match record.kind {
            TarRecordKind::File => {
                chown_if_needed(&path, record.uid, record.gid)?;
                chmod(&path, record.mode)?;
            }
            TarRecordKind::Symlink => {
                lchown_if_needed(&path, record.uid, record.gid)?;
            }
            TarRecordKind::Directory => {
                directories.push((record.path.clone(), record.uid, record.gid, record.mode));
            }
        }
    }

    directories.sort_by_key(|(path, _, _, _)| std::cmp::Reverse(path_depth(path)));
    for (path, uid, gid, mode) in directories {
        let dir = root.join(path);
        chown_if_needed(&dir, uid, gid)?;
        chmod(&dir, mode)?;
    }
    chmod(root, 0o755)?;
    Ok(())
}

fn path_depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.split('/').count()
    }
}

fn chown_if_needed(path: &Path, uid: u32, gid: u32) -> Result<(), OciExtractError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| map_io(path, "inspect before chown", error))?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }
    chown(path, uid, gid, "chown")
}

fn lchown_if_needed(path: &Path, uid: u32, gid: u32) -> Result<(), OciExtractError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| map_io(path, "inspect before lchown", error))?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }
    chown(path, uid, gid, "lchown")
}

fn chown(path: &Path, uid: u32, gid: u32, operation: &str) -> Result<(), OciExtractError> {
    let c_path = c_path(path, operation)?;
    let result = if operation == "lchown" {
        unsafe { libc::lchown(c_path.as_ptr(), uid, gid) }
    } else {
        unsafe { libc::chown(c_path.as_ptr(), uid, gid) }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(map_io(path, operation, io::Error::last_os_error()))
    }
}

fn c_path(path: &Path, operation: &str) -> Result<CString, OciExtractError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|error| {
        OciExtractError::Io(format!(
            "failed to convert path '{}' for {operation}: {error}",
            path.display()
        ))
    })
}

fn chmod(path: &Path, mode: u32) -> Result<(), OciExtractError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| map_io(path, "chmod", error))
}

fn map_io(path: &Path, operation: &str, error: io::Error) -> OciExtractError {
    OciExtractError::Io(format!(
        "failed to {operation} '{}': {error}",
        path.display()
    ))
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
    use crate::{Builder, TypedBuilder};
    use bobr_store::fs_tree::{FsTreeEntry, FsTreeManifest};
    use bobr_store::{Store, import_build};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    #[test]
    fn input_spec_is_registered_shape() {
        assert_eq!(TypedBuilder::tag(&OciExtractBuilder), "OciExtract");
        assert_eq!(OCI_EXTRACT_SPEC.required_inputs, &["image"]);
        assert!(!OCI_EXTRACT_SPEC.allow_extra_inputs);
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

        let records = extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap();

        assert_eq!(fs::read(root.join("bin/tool")).unwrap(), b"hello\n");
        assert_eq!(
            fs::read_link(root.join("tool-link")).unwrap(),
            Path::new("bin/tool")
        );
        assert!(records.iter().any(|record| {
            record.path == "bin/tool"
                && record.kind == TarRecordKind::File
                && record.uid == 1
                && record.gid == 2
                && record.mode == 0o755
        }));
        assert!(records.iter().any(|record| {
            record.path == "tool-link"
                && record.kind == TarRecordKind::Symlink
                && record.uid == 3
                && record.gid == 4
                && record.symlink_target.as_deref() == Some("bin/tool")
        }));
    }

    #[test]
    fn extracts_uncompressed_tar_layer() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| append_file(builder, "file", b"plain", 0, 0, 0o644));
        let oci = create_oci_layout(temp.path(), vec![(MEDIA_TYPE_OCI_LAYER_TAR, tar)]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap();

        assert_eq!(fs::read(root.join("file")).unwrap(), b"plain");
    }

    #[test]
    fn rejects_path_traversal_entries() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| append_raw_file(builder, "../escape", b"bad", 0, 0, 0o644));
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let error = extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap_err();

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

        let records = extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap();

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

        let records = extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap();

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

        let records = extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap();

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
    fn preserves_compatible_tar_hardlinks() {
        let temp = tempdir().unwrap();
        let tar = make_tar(|builder| {
            append_file(builder, "bin/tool", b"tool", 0, 0, 0o755);
            append_hardlink(builder, "bin/tool2", "bin/tool", 0, 0, 0o755);
        });
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let records = extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap();

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

        let error = extract_oci_image_layers(&oci, &root, &mut Vec::new()).unwrap_err();

        assert!(error.to_string().contains("different from target"));
    }

    #[test]
    fn builder_imports_as_fs_tree_manifest() {
        let temp = tempdir().unwrap();
        let store_root = temp.path().join("store");
        fs::create_dir(&store_root).unwrap();
        let store = Store::create(&store_root).unwrap();
        let owner = fs::symlink_metadata(temp.path()).unwrap();
        let mut cx = build_context(temp.path(), store.fs_tree());
        let tar = make_tar(|builder| {
            append_dir(
                builder,
                "bin",
                owner.uid() as u64,
                owner.gid() as u64,
                0o755,
            );
            append_file(
                builder,
                "bin/tool",
                b"tool",
                owner.uid() as u64,
                owner.gid() as u64,
                0o755,
            );
            append_symlink(
                builder,
                "tool-link",
                "bin/tool",
                owner.uid() as u64,
                owner.gid() as u64,
            );
        });
        let oci = create_oci_layout(temp.path(), vec![(oci::MEDIA_TYPE_OCI_LAYER, gzip(&tar))]);
        let mut inputs = BuilderInputs::empty();
        inputs.insert("image", oci);

        let result = OciExtractBuilder
            .build_typed(OciExtractConfig {}, inputs, &mut cx)
            .unwrap();
        assert_eq!(
            result.staged_path,
            temp.path().join("tmp").join(OUTPUT_MANIFEST_FILE_NAME)
        );
        assert!(!temp.path().join("tmp").join(EXTRACT_ROOT_DIR_NAME).exists());

        let manifest = FsTreeManifest::read_canonical(&result.staged_path).unwrap();
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
            0o755,
        )));
        assert!(manifest.entries().contains(&FsTreeEntry::symlink(
            "tool-link",
            owner.uid(),
            owner.gid(),
            "bin/tool",
        )));
        assert!(manifest.entries().iter().any(|entry| matches!(
            entry,
            FsTreeEntry::File { path, .. } if path == "bin/tool"
        )));
        let manifest_hash = import_build(
            &store,
            "0".repeat(64).parse().unwrap(),
            "0".repeat(64).parse().unwrap(),
            Vec::new(),
            &result.staged_path,
            "staged-object",
        )
        .unwrap();
        let root = store
            .fs_tree()
            .ensure_materialized_root(None, manifest_hash)
            .unwrap();
        assert_eq!(fs::read(root.join("bin/tool")).unwrap(), b"tool");
        assert_eq!(
            fs::read_link(root.join("tool-link")).unwrap(),
            Path::new("bin/tool")
        );
    }

    #[test]
    fn metadata_application_applies_directory_modes_last() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let private = root.join("private");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&private).unwrap();
        fs::write(private.join("file"), b"secret").unwrap();
        let owner = fs::symlink_metadata(temp.path()).unwrap();
        let records = vec![
            TarEntryRecord {
                path: "private".to_string(),
                kind: TarRecordKind::Directory,
                uid: owner.uid(),
                gid: owner.gid(),
                mode: 0o000,
                symlink_target: None,
            },
            TarEntryRecord {
                path: "private/file".to_string(),
                kind: TarRecordKind::File,
                uid: owner.uid(),
                gid: owner.gid(),
                mode: 0o600,
                symlink_target: None,
            },
        ];

        apply_extracted_metadata(&root, &records).unwrap();

        assert_eq!(
            fs::symlink_metadata(&private).unwrap().permissions().mode() & 0o7777,
            0o000
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            fs::symlink_metadata(private.join("file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path(), FsTree::new(temp.path().to_path_buf()));
        let mut inputs = BuilderInputs::empty();
        let image = create_oci_layout(temp.path(), vec![]);
        inputs.insert("image", image);

        let error = OciExtractBuilder
            .build_erased(serde_json::json!({ "unexpected": true }), inputs, &mut cx)
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
    }

    fn build_context(root: &Path, fs_tree: FsTree) -> BuildContext {
        let temp_dir = root.join("tmp");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        BuildContext::with_noop_logger(temp_dir, fs_tree)
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
        let manifest = OciManifest {
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
