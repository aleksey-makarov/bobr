use flate2::read::GzDecoder;
use mbuild_core::{Builder, BuilderError};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::env;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tar::Archive;
use xz2::read::XzDecoder;
use zip::read::ZipArchive;

const ROOT_DIR: &str = ".mbuild";
const BUILDER_DIR: &str = "fetch";
const CACHE_DIR: &str = "cache";
const OBJECTS_DIR: &str = "objects";
const META_DIR: &str = "meta";
const REFS_DIR: &str = "refs";
const REDIRECT_LIMIT: usize = 10;

#[derive(Debug)]
enum FetchError {
    InvalidRecipe(String),
    NetworkFailed(String),
    HashMismatch(String),
    ExtractFailed(String),
    FsFailed(String),
}

impl FetchError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidRecipe(message)
            | Self::NetworkFailed(message)
            | Self::HashMismatch(message)
            | Self::ExtractFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

type FResult<T> = Result<T, FetchError>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LayoutKind {
    File,
    ArchiveTree,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ArchiveFormat {
    TarGz,
    TarXz,
    Zip,
}

#[derive(Debug, Deserialize)]
struct FetchRecipe {
    #[serde(rename = "type")]
    recipe_type: String,
    url: String,
    hash: String,
    layout: LayoutKind,
    #[serde(default)]
    archive_format: Option<ArchiveFormat>,
    #[serde(default)]
    artifact_kind: Option<String>,
    #[serde(default)]
    outputs: Vec<String>,
}

pub struct FetchBuilder;

impl Builder for FetchBuilder {
    fn get_type(&self) -> &'static str {
        "fetch"
    }

    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError> {
        let recipe = parse_recipe(recipe)?;
        let layout = workspace_layout().map_err(map_error)?;
        ensure_base_dirs(&layout).map_err(map_error)?;

        let outputs = output_ids(artifact, &recipe);
        let expected_hash = parse_hash(&recipe.hash).map_err(map_error)?;

        let cached_blob =
            ensure_cached_blob(&layout, &recipe.url, &expected_hash).map_err(map_error)?;

        let artifact_kind = recipe
            .artifact_kind
            .clone()
            .unwrap_or_else(|| match recipe.layout {
                LayoutKind::File => "fetched-file".to_string(),
                LayoutKind::ArchiveTree => "source-tree".to_string(),
            });

        for output_id in &outputs {
            publish_output(&layout, output_id, &recipe, &artifact_kind, &cached_blob)
                .map_err(map_error)?;
        }

        println!("build: ok");
        println!("artifact: {artifact}");
        println!("url: {}", recipe.url);
        println!("hash: {}", recipe.hash);
        println!("layout: {}", layout_name(&recipe.layout));
        println!("outputs: {}", outputs.join(", "));
        Ok(())
    }

    fn summarize_recipe(
        &self,
        recipe: &Value,
    ) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let recipe = parse_recipe(recipe)?;
        Ok(vec![
            ("url", recipe.url),
            ("hash", recipe.hash),
            ("layout", layout_name(&recipe.layout).to_string()),
        ])
    }
}

fn layout_name(layout: &LayoutKind) -> &'static str {
    match layout {
        LayoutKind::File => "file",
        LayoutKind::ArchiveTree => "archive-tree",
    }
}

fn output_ids(artifact: &str, recipe: &FetchRecipe) -> Vec<String> {
    if recipe.outputs.is_empty() {
        vec![artifact.to_string()]
    } else {
        recipe.outputs.clone()
    }
}

fn parse_recipe(value: &Value) -> Result<FetchRecipe, BuilderError> {
    serde_json::from_value::<FetchRecipe>(value.clone())
        .map_err(|error| BuilderError::InvalidRecipe(format!("invalid fetch recipe: {error}")))
        .and_then(|recipe| {
            validate_recipe(&recipe).map_err(map_error)?;
            Ok(recipe)
        })
}

fn validate_recipe(recipe: &FetchRecipe) -> FResult<()> {
    if recipe.recipe_type != "fetch" {
        return Err(FetchError::InvalidRecipe(
            "type must be 'fetch'".to_string(),
        ));
    }

    if !recipe.url.starts_with("http://") && !recipe.url.starts_with("https://") {
        return Err(FetchError::InvalidRecipe(
            "url must start with http:// or https://".to_string(),
        ));
    }

    parse_hash(&recipe.hash)?;

    if matches!(recipe.layout, LayoutKind::ArchiveTree) && recipe.archive_format.is_none() {
        return Err(FetchError::InvalidRecipe(
            "archive_format is required when layout = 'archive-tree'".to_string(),
        ));
    }

    if matches!(recipe.layout, LayoutKind::File) && recipe.archive_format.is_some() {
        return Err(FetchError::InvalidRecipe(
            "archive_format must not be set when layout = 'file'".to_string(),
        ));
    }

    for output in &recipe.outputs {
        validate_name(output)?;
    }

    Ok(())
}

fn validate_name(name: &str) -> FResult<()> {
    if name.is_empty() {
        return Err(FetchError::InvalidRecipe(
            "artifact/output name must not be empty".to_string(),
        ));
    }

    if name == "." || name == ".." {
        return Err(FetchError::InvalidRecipe(format!(
            "invalid artifact/output name '{name}'"
        )));
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(FetchError::InvalidRecipe(format!(
            "invalid artifact/output name '{}'; allowed chars: [A-Za-z0-9._-]",
            name
        )));
    }

    Ok(())
}

#[derive(Debug)]
struct ParsedHash {
    algorithm: String,
    value: String,
}

fn parse_hash(value: &str) -> FResult<ParsedHash> {
    let (algorithm, hash_value) = value.split_once(':').ok_or_else(|| {
        FetchError::InvalidRecipe(
            "hash must be in form '<algo>:<hex>' (supported: md5, sha256)".to_string(),
        )
    })?;
    let normalized_algorithm = algorithm.to_lowercase();

    match normalized_algorithm.as_str() {
        "md5" => {
            if hash_value.len() != 32 || !hash_value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(FetchError::InvalidRecipe(
                    "md5 hash must be 32 hex characters".to_string(),
                ));
            }
        }
        "sha256" => {
            if hash_value.len() != 64 || !hash_value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(FetchError::InvalidRecipe(
                    "sha256 hash must be 64 hex characters".to_string(),
                ));
            }
        }
        _ => {
            return Err(FetchError::InvalidRecipe(
                "unsupported hash algorithm; supported: md5, sha256".to_string(),
            ));
        }
    }

    Ok(ParsedHash {
        algorithm: normalized_algorithm,
        value: hash_value.to_lowercase(),
    })
}

fn ensure_cached_blob(layout: &WorkspaceLayout, url: &str, hash: &ParsedHash) -> FResult<PathBuf> {
    let algo_dir = layout.cache.join(&hash.algorithm);
    ensure_dir(&algo_dir, "fetch cache algo")?;
    let cache_path = algo_dir.join(format!("{}.blob", hash.value));

    if cache_path.exists() {
        let existing_hash = compute_hash(&cache_path, &hash.algorithm)?;
        if existing_hash == hash.value {
            println!("cache: hit");
            println!("cached_blob: {}", cache_path.display());
            return Ok(cache_path);
        }

        fs::remove_file(&cache_path).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to remove corrupted cache blob '{}': {error}",
                cache_path.display()
            ))
        })?;
    }

    println!("cache: miss");
    let now_nanos = current_epoch_nanos()?;
    let tmp_path = layout
        .builder_root
        .join(format!(".download-{}-{}.blob", hash.value, now_nanos));

    download_to_file(url, &tmp_path)?;

    let downloaded_hash = compute_hash(&tmp_path, &hash.algorithm)?;
    if downloaded_hash != hash.value {
        let _ = fs::remove_file(&tmp_path);
        return Err(FetchError::HashMismatch(format!(
            "downloaded file hash mismatch: expected {}, got {}",
            hash.value, downloaded_hash
        )));
    }

    fs::rename(&tmp_path, &cache_path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to move downloaded blob '{}' -> '{}': {error}",
            tmp_path.display(),
            cache_path.display()
        ))
    })?;

    println!("cached_blob: {}", cache_path.display());
    Ok(cache_path)
}

fn download_to_file(url: &str, destination: &Path) -> FResult<()> {
    let client = Client::builder()
        .redirect(Policy::limited(REDIRECT_LIMIT))
        .build()
        .map_err(|error| {
            FetchError::NetworkFailed(format!("failed to create HTTP client: {error}"))
        })?;

    let mut response = client.get(url).send().map_err(|error| {
        FetchError::NetworkFailed(format!("failed to download '{url}': {error}"))
    })?;

    if !response.status().is_success() {
        return Err(FetchError::NetworkFailed(format!(
            "failed to download '{}': HTTP {}",
            url,
            response.status()
        )));
    }

    let mut file = File::create(destination).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to create temporary download file '{}': {error}",
            destination.display()
        ))
    })?;

    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read_bytes = response.read(&mut buffer).map_err(|error| {
            FetchError::NetworkFailed(format!("failed to read HTTP response body: {error}"))
        })?;
        if read_bytes == 0 {
            break;
        }
        file.write_all(&buffer[..read_bytes]).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to write temporary download file '{}': {error}",
                destination.display()
            ))
        })?;
    }

    Ok(())
}

fn compute_hash(path: &Path, algorithm: &str) -> FResult<String> {
    match algorithm {
        "sha256" => compute_sha256(path),
        "md5" => compute_md5(path),
        _ => Err(FetchError::InvalidRecipe(format!(
            "unsupported hash algorithm '{}'",
            algorithm
        ))),
    }
}

fn compute_sha256(path: &Path) -> FResult<String> {
    let mut file = File::open(path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to open file for hashing '{}': {error}",
            path.display()
        ))
    })?;

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read_bytes = file.read(&mut buffer).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to read file for hashing '{}': {error}",
                path.display()
            ))
        })?;
        if read_bytes == 0 {
            break;
        }
        hasher.update(&buffer[..read_bytes]);
    }

    let digest = hasher.finalize();
    Ok(bytes_to_hex(&digest))
}

fn compute_md5(path: &Path) -> FResult<String> {
    let mut file = File::open(path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to open file for hashing '{}': {error}",
            path.display()
        ))
    })?;

    let mut context = md5::Context::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read_bytes = file.read(&mut buffer).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to read file for hashing '{}': {error}",
                path.display()
            ))
        })?;
        if read_bytes == 0 {
            break;
        }
        context.consume(&buffer[..read_bytes]);
    }

    let digest = context.compute();
    Ok(bytes_to_hex(&digest.0))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(hex_char(b >> 4));
        out.push(hex_char(b & 0x0f));
    }
    out
}

fn hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => '0',
    }
}

fn publish_output(
    layout: &WorkspaceLayout,
    output_id: &str,
    recipe: &FetchRecipe,
    artifact_kind: &str,
    cached_blob: &Path,
) -> FResult<()> {
    validate_name(output_id)?;

    match recipe.layout {
        LayoutKind::File => {
            publish_file_output(layout, output_id, recipe, artifact_kind, cached_blob)
        }
        LayoutKind::ArchiveTree => {
            publish_archive_output(layout, output_id, recipe, artifact_kind, cached_blob)
        }
    }
}

fn publish_file_output(
    layout: &WorkspaceLayout,
    output_id: &str,
    recipe: &FetchRecipe,
    artifact_kind: &str,
    cached_blob: &Path,
) -> FResult<()> {
    let now_nanos = current_epoch_nanos()?;
    let tmp_path = layout
        .root
        .join(format!(".fetch-file-{}-{}.tmp", output_id, now_nanos));

    if tmp_path.exists() {
        fs::remove_file(&tmp_path).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to remove previous temporary file '{}': {error}",
                tmp_path.display()
            ))
        })?;
    }

    fs::copy(cached_blob, &tmp_path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to copy cached blob '{}' to '{}': {error}",
            cached_blob.display(),
            tmp_path.display()
        ))
    })?;

    let object_path = layout.objects.join(output_id);
    replace_path(&tmp_path, &object_path)?;

    let meta_path = layout.meta.join(format!("{output_id}.ncl"));
    write_atomic(
        &meta_path,
        &render_meta_ncl(output_id, artifact_kind, recipe, object_path.as_path()),
    )?;

    let ref_path = layout.refs.join(output_id);
    let ref_target = PathBuf::from("..").join(OBJECTS_DIR).join(output_id);
    replace_symlink(&ref_target, &ref_path)?;

    println!("publish: ok");
    println!("output: {output_id}");
    println!("object: {}", object_path.display());
    println!("meta: {}", meta_path.display());
    println!("ref: {}", ref_path.display());

    Ok(())
}

fn publish_archive_output(
    layout: &WorkspaceLayout,
    output_id: &str,
    recipe: &FetchRecipe,
    artifact_kind: &str,
    cached_blob: &Path,
) -> FResult<()> {
    let now_nanos = current_epoch_nanos()?;
    let tmp_dir = layout
        .root
        .join(format!(".fetch-archive-{}-{}.dir", output_id, now_nanos));

    recreate_empty_dir(&tmp_dir)?;
    extract_archive(
        cached_blob,
        recipe
            .archive_format
            .clone()
            .expect("validated archive_format"),
        &tmp_dir,
    )?;

    let object_path = layout.objects.join(output_id);
    replace_path(&tmp_dir, &object_path)?;

    let meta_path = layout.meta.join(format!("{output_id}.ncl"));
    write_atomic(
        &meta_path,
        &render_meta_ncl(output_id, artifact_kind, recipe, object_path.as_path()),
    )?;

    let ref_path = layout.refs.join(output_id);
    let ref_target = PathBuf::from("..").join(OBJECTS_DIR).join(output_id);
    replace_symlink(&ref_target, &ref_path)?;

    println!("publish: ok");
    println!("output: {output_id}");
    println!("object: {}", object_path.display());
    println!("meta: {}", meta_path.display());
    println!("ref: {}", ref_path.display());

    Ok(())
}

fn extract_archive(archive_path: &Path, format: ArchiveFormat, destination: &Path) -> FResult<()> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, destination),
        ArchiveFormat::TarXz => extract_tar_xz(archive_path, destination),
        ArchiveFormat::Zip => extract_zip(archive_path, destination),
    }
}

fn extract_tar_gz(archive_path: &Path, destination: &Path) -> FResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        FetchError::ExtractFailed(format!(
            "failed to open tar.gz archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn extract_tar_xz(archive_path: &Path, destination: &Path) -> FResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        FetchError::ExtractFailed(format!(
            "failed to open tar.xz archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    let decoder = XzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn unpack_tar_safely<R: Read>(archive: &mut Archive<R>, destination: &Path) -> FResult<()> {
    let entries = archive.entries().map_err(|error| {
        FetchError::ExtractFailed(format!("failed to read tar archive entries: {error}"))
    })?;

    for entry_result in entries {
        let mut entry = entry_result.map_err(|error| {
            FetchError::ExtractFailed(format!("failed to parse tar entry: {error}"))
        })?;

        entry.unpack_in(destination).map_err(|error| {
            FetchError::ExtractFailed(format!(
                "failed to extract tar entry into '{}': {error}",
                destination.display()
            ))
        })?;
    }

    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> FResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        FetchError::ExtractFailed(format!(
            "failed to open zip archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    let mut zip = ZipArchive::new(file).map_err(|error| {
        FetchError::ExtractFailed(format!(
            "failed to open zip archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| {
            FetchError::ExtractFailed(format!("failed to read zip entry #{index}: {error}"))
        })?;

        let enclosed = entry.enclosed_name().ok_or_else(|| {
            FetchError::ExtractFailed(format!(
                "zip entry '{}' has invalid or unsafe path",
                entry.name()
            ))
        })?;

        let target_path = destination.join(enclosed);
        if !target_path
            .components()
            .all(|c| !matches!(c, Component::ParentDir | Component::RootDir))
        {
            return Err(FetchError::ExtractFailed(format!(
                "zip entry '{}' resolves to unsafe path",
                entry.name()
            )));
        }

        if entry.is_dir() {
            fs::create_dir_all(&target_path).map_err(|error| {
                FetchError::ExtractFailed(format!(
                    "failed to create directory '{}': {error}",
                    target_path.display()
                ))
            })?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                FetchError::ExtractFailed(format!(
                    "failed to create parent directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }

        let mut out = File::create(&target_path).map_err(|error| {
            FetchError::ExtractFailed(format!(
                "failed to create file '{}': {error}",
                target_path.display()
            ))
        })?;

        std::io::copy(&mut entry, &mut out).map_err(|error| {
            FetchError::ExtractFailed(format!(
                "failed to extract zip entry '{}' to '{}': {error}",
                entry.name(),
                target_path.display()
            ))
        })?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            fs::set_permissions(&target_path, fs::Permissions::from_mode(mode)).map_err(
                |error| {
                    FetchError::ExtractFailed(format!(
                        "failed to set permissions on '{}': {error}",
                        target_path.display()
                    ))
                },
            )?;
        }
    }

    Ok(())
}

fn render_meta_ncl(
    id: &str,
    artifact_kind: &str,
    recipe: &FetchRecipe,
    object_path: &Path,
) -> String {
    let object_kind = if object_path.is_dir() {
        "directory"
    } else {
        "file"
    };
    let archive_format = recipe
        .archive_format
        .as_ref()
        .map(|fmt| match fmt {
            ArchiveFormat::TarGz => "tar-gz",
            ArchiveFormat::TarXz => "tar-xz",
            ArchiveFormat::Zip => "zip",
        })
        .unwrap_or("");

    format!(
        "{{\n  id = {},\n  artifact_kind = {},\n  producer = {{\n    builder = \"fetch\",\n    url = {},\n    hash = {},\n  }},\n  attrs = {{\n    layout = {},\n    archive_format = {},\n    object_kind = {},\n  }},\n}}\n",
        q(id),
        q(artifact_kind),
        q(&recipe.url),
        q(&recipe.hash),
        q(layout_name(&recipe.layout)),
        q(archive_format),
        q(object_kind),
    )
}

fn recreate_empty_dir(path: &Path) -> FResult<()> {
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path).map_err(|error| {
                FetchError::FsFailed(format!(
                    "failed to remove previous directory '{}': {error}",
                    path.display()
                ))
            })?;
        } else {
            fs::remove_file(path).map_err(|error| {
                FetchError::FsFailed(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

fn replace_path(tmp_path: &Path, destination: &Path) -> FResult<()> {
    if destination.exists() {
        if destination.is_dir() {
            fs::remove_dir_all(destination).map_err(|error| {
                FetchError::FsFailed(format!(
                    "failed to remove existing directory '{}': {error}",
                    destination.display()
                ))
            })?;
        } else {
            fs::remove_file(destination).map_err(|error| {
                FetchError::FsFailed(format!(
                    "failed to remove existing file '{}': {error}",
                    destination.display()
                ))
            })?;
        }
    }

    fs::rename(tmp_path, destination).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to publish '{}' -> '{}': {error}",
            tmp_path.display(),
            destination.display()
        ))
    })
}

fn replace_symlink(target: &Path, link_path: &Path) -> FResult<()> {
    if link_path.exists() || link_path.is_symlink() {
        let metadata = fs::symlink_metadata(link_path).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to inspect existing ref '{}': {error}",
                link_path.display()
            ))
        })?;

        if metadata.file_type().is_dir() {
            fs::remove_dir_all(link_path).map_err(|error| {
                FetchError::FsFailed(format!(
                    "failed to remove existing ref directory '{}': {error}",
                    link_path.display()
                ))
            })?;
        } else {
            fs::remove_file(link_path).map_err(|error| {
                FetchError::FsFailed(format!(
                    "failed to remove existing ref '{}': {error}",
                    link_path.display()
                ))
            })?;
        }
    }

    create_symlink(target, link_path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> FResult<()> {
    unix_fs::symlink(target, link_path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to create ref symlink '{}' -> '{}': {error}",
            link_path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link_path: &Path) -> FResult<()> {
    Err(FetchError::FsFailed(
        "symlink refs are currently supported only on unix hosts".to_string(),
    ))
}

fn write_atomic(path: &Path, content: &str) -> FResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            FetchError::FsFailed(format!(
                "invalid file name for atomic write path '{}'",
                path.display()
            ))
        })?;

    let tmp_name = format!(".{file_name}.tmp");
    let tmp_path = path.with_file_name(tmp_name);

    fs::write(&tmp_path, content).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to write temporary file '{}': {error}",
            tmp_path.display()
        ))
    })?;

    fs::rename(&tmp_path, path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to move temporary file '{}' to '{}': {error}",
            tmp_path.display(),
            path.display()
        ))
    })
}

fn q(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<serialization-error>\"".to_string())
}

fn current_epoch_nanos() -> FResult<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| FetchError::FsFailed(format!("system time before UNIX_EPOCH: {error}")))
}

struct WorkspaceLayout {
    root: PathBuf,
    builder_root: PathBuf,
    cache: PathBuf,
    objects: PathBuf,
    meta: PathBuf,
    refs: PathBuf,
}

fn workspace_layout() -> FResult<WorkspaceLayout> {
    let cwd = env::current_dir().map_err(|error| {
        FetchError::FsFailed(format!("failed to get current directory: {error}"))
    })?;
    let root = cwd.join(ROOT_DIR);
    let builder_root = root.join(BUILDER_DIR);

    Ok(WorkspaceLayout {
        root: root.clone(),
        builder_root: builder_root.clone(),
        cache: builder_root.join(CACHE_DIR),
        objects: root.join(OBJECTS_DIR),
        meta: root.join(META_DIR),
        refs: root.join(REFS_DIR),
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> FResult<()> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.builder_root, "fetch builder root")?;
    ensure_dir(&layout.cache, "fetch cache")?;
    ensure_dir(&layout.objects, "objects")?;
    ensure_dir(&layout.meta, "meta")?;
    ensure_dir(&layout.refs, "refs")?;
    Ok(())
}

fn ensure_dir(path: &Path, label: &str) -> FResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

fn map_error(error: FetchError) -> BuilderError {
    match error {
        FetchError::InvalidRecipe(message) => BuilderError::InvalidRecipe(message),
        FetchError::NetworkFailed(message)
        | FetchError::HashMismatch(message)
        | FetchError::ExtractFailed(message)
        | FetchError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}
