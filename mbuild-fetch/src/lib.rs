use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use mbuild_core::{
    BuildContext, BuilderError, BuilderSpec, InputSlot, ProducerInfo, ResolvedInputs,
    StagedBuildResult, TypedBuilder, fsutil,
};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use tar::Archive;
use xz2::read::XzDecoder;
use zip::read::ZipArchive;

const CACHE_DIR: &str = "cache";
const REDIRECT_LIMIT: usize = 10;
const USER_AGENT: &str = "mbuild-fetch/0.1";

#[derive(Debug)]
enum FetchError {
    InvalidConfig(String),
    NetworkFailed(String),
    ExtractFailed(String),
    FsFailed(String),
}

impl FetchError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::NetworkFailed(message)
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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ArchiveFormat {
    TarGz,
    TarXz,
    TarBz2,
    Zip,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum UrlField {
    One(String),
    Many(Vec<String>),
}

impl UrlField {
    fn as_list(&self) -> Vec<String> {
        match self {
            Self::One(url) => vec![url.clone()],
            Self::Many(urls) => urls.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchConfig {
    url: UrlField,
    hash: String,
    #[serde(default = "default_unpack")]
    unpack: bool,
    #[serde(default)]
    archive_format: Option<ArchiveFormat>,
    #[serde(default)]
    kind: Option<String>,
}

fn default_unpack() -> bool {
    true
}

#[derive(Debug)]
struct ParsedHash {
    algorithm: String,
    value: String,
}

pub struct FetchBuilder;

static FETCH_SPEC: BuilderSpec = BuilderSpec {
    tag: "Fetch",
    inputs: &[] as &[InputSlot],
};

impl TypedBuilder for FetchBuilder {
    type Config = FetchConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &FETCH_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: ResolvedInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_config(&config).map_err(map_error)?;
        if !inputs.is_empty() {
            return Err(BuilderError::ExecutionFailed(
                "Fetch builder does not accept input objects".to_string(),
            ));
        }

        let cache_dir = cx.builder_root.join(CACHE_DIR);
        ensure_dir(&cx.builder_root, "fetch builder root").map_err(map_error)?;
        ensure_dir(&cache_dir, "fetch cache").map_err(map_error)?;
        ensure_dir(&cx.temp_root, "fetch temp").map_err(map_error)?;

        let urls = config.url.as_list();
        let expected_hash = parse_hash(&config.hash).map_err(map_error)?;
        let (cached_blob, source_url) =
            ensure_cached_blob(&cache_dir, &cx.builder_root, &urls, &expected_hash)
                .map_err(map_error)?;

        let kind = config.kind.clone().unwrap_or_else(|| {
            if config.unpack {
                "source-tree".to_string()
            } else {
                "fetched-file".to_string()
            }
        });

        let (staged_path, mut attrs) = if config.unpack {
            let format =
                select_archive_format(&config, &cached_blob, &source_url).map_err(map_error)?;
            let (path, normalized_root) =
                stage_archive_output(&cx.temp_root, &cached_blob, format.clone())
                    .map_err(map_error)?;
            let mut attrs = Map::new();
            attrs.insert(
                "archive_format".to_string(),
                Value::String(archive_format_name(&format).to_string()),
            );
            attrs.insert("normalized_root".to_string(), Value::Bool(normalized_root));
            attrs.insert("unpack".to_string(), Value::Bool(true));
            (path, attrs)
        } else {
            let path = stage_file_output(&cx.temp_root, &cached_blob).map_err(map_error)?;
            let mut attrs = Map::new();
            attrs.insert("unpack".to_string(), Value::Bool(false));
            (path, attrs)
        };

        attrs.insert(
            "source_url".to_string(),
            Value::String(source_url.to_string()),
        );
        attrs.insert(
            "declared_hash".to_string(),
            Value::String(config.hash.clone()),
        );

        Ok(StagedBuildResult {
            kind,
            producer: ProducerInfo {
                builder: "fetch".to_string(),
            },
            input_build_keys: vec![],
            attrs,
            staged_path,
        })
    }
}

fn validate_config(config: &FetchConfig) -> FResult<()> {
    let urls = config.url.as_list();
    if urls.is_empty() {
        return Err(FetchError::InvalidConfig(
            "url list must not be empty".to_string(),
        ));
    }
    for url in &urls {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(FetchError::InvalidConfig(format!(
                "url '{}' must start with http:// or https://",
                url
            )));
        }
    }

    parse_hash(&config.hash)?;

    if !config.unpack && config.archive_format.is_some() {
        return Err(FetchError::InvalidConfig(
            "archive_format must not be set when unpack = false".to_string(),
        ));
    }

    Ok(())
}

fn parse_hash(value: &str) -> FResult<ParsedHash> {
    let (algorithm, hash_value) = value.split_once(':').ok_or_else(|| {
        FetchError::InvalidConfig(
            "hash must be in form '<algo>:<hex>' (supported: md5, sha256)".to_string(),
        )
    })?;
    let normalized_algorithm = algorithm.to_lowercase();

    match normalized_algorithm.as_str() {
        "md5" => {
            if hash_value.len() != 32 || !hash_value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(FetchError::InvalidConfig(
                    "md5 hash must be 32 hex characters".to_string(),
                ));
            }
        }
        "sha256" => {
            if hash_value.len() != 64 || !hash_value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(FetchError::InvalidConfig(
                    "sha256 hash must be 64 hex characters".to_string(),
                ));
            }
        }
        _ => {
            return Err(FetchError::InvalidConfig(
                "unsupported hash algorithm; supported: md5, sha256".to_string(),
            ));
        }
    }

    Ok(ParsedHash {
        algorithm: normalized_algorithm,
        value: hash_value.to_lowercase(),
    })
}

fn ensure_cached_blob(
    cache_root: &Path,
    builder_root: &Path,
    urls: &[String],
    hash: &ParsedHash,
) -> FResult<(PathBuf, String)> {
    let algo_dir = cache_root.join(&hash.algorithm);
    ensure_dir(&algo_dir, "fetch cache algo")?;
    let cache_path = algo_dir.join(format!("{}.blob", hash.value));

    if cache_path.exists() {
        let existing_hash = compute_hash(&cache_path, &hash.algorithm)?;
        if existing_hash == hash.value {
            return Ok((cache_path, urls[0].clone()));
        }

        fs::remove_file(&cache_path).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to remove corrupted cache blob '{}': {error}",
                cache_path.display()
            ))
        })?;
    }

    let mut failures = Vec::new();

    for (index, url) in urls.iter().enumerate() {
        let now_nanos = fsutil::current_epoch_nanos().map_err(map_fsutil_error)?;
        let tmp_path = builder_root.join(format!(
            ".download-{}-{}-{}.blob",
            hash.value, index, now_nanos
        ));

        if let Err(error) = download_to_file(url, &tmp_path) {
            failures.push(format!("{url}: {}", error.message()));
            let _ = fs::remove_file(&tmp_path);
            continue;
        }

        let downloaded_hash = compute_hash(&tmp_path, &hash.algorithm)?;
        if downloaded_hash != hash.value {
            failures.push(format!(
                "{url}: hash mismatch (expected {}, got {})",
                hash.value, downloaded_hash
            ));
            let _ = fs::remove_file(&tmp_path);
            continue;
        }

        fs::rename(&tmp_path, &cache_path).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to move downloaded blob '{}' -> '{}': {error}",
                tmp_path.display(),
                cache_path.display()
            ))
        })?;

        return Ok((cache_path, url.clone()));
    }

    Err(FetchError::NetworkFailed(format!(
        "all download URLs failed:\n  - {}",
        failures.join("\n  - ")
    )))
}

fn download_to_file(url: &str, destination: &Path) -> FResult<()> {
    let client = Client::builder()
        .redirect(Policy::limited(REDIRECT_LIMIT))
        .user_agent(USER_AGENT)
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
        _ => Err(FetchError::InvalidConfig(format!(
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

fn stage_file_output(temp_root: &Path, cached_blob: &Path) -> FResult<PathBuf> {
    let tmp_path = temp_root.join("fetch-file.out");
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

    Ok(tmp_path)
}

fn stage_archive_output(
    temp_root: &Path,
    cached_blob: &Path,
    format: ArchiveFormat,
) -> FResult<(PathBuf, bool)> {
    let tmp_dir = temp_root.join("fetch-archive.dir");
    fsutil::recreate_empty_dir(&tmp_dir).map_err(map_fsutil_error)?;
    extract_archive(cached_blob, format, &tmp_dir)?;
    let normalized_root = normalize_extracted_root(&tmp_dir)?;
    Ok((tmp_dir, normalized_root))
}

fn extract_archive(archive_path: &Path, format: ArchiveFormat, destination: &Path) -> FResult<()> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, destination),
        ArchiveFormat::TarXz => extract_tar_xz(archive_path, destination),
        ArchiveFormat::TarBz2 => extract_tar_bz2(archive_path, destination),
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

fn extract_tar_bz2(archive_path: &Path, destination: &Path) -> FResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        FetchError::ExtractFailed(format!(
            "failed to open tar.bz2 archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    let decoder = BzDecoder::new(file);
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

fn normalize_extracted_root(directory: &Path) -> FResult<bool> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to read extracted directory '{}': {error}",
                directory.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to list extracted directory '{}': {error}",
                directory.display()
            ))
        })?;

    if entries.len() != 1 {
        return Ok(false);
    }

    let only_entry = entries.remove(0);
    let only_entry_path = only_entry.path();
    let only_entry_file_type = only_entry.file_type().map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to inspect extracted entry '{}': {error}",
            only_entry_path.display()
        ))
    })?;
    if !only_entry_file_type.is_dir() {
        return Ok(false);
    }

    for child in fs::read_dir(&only_entry_path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to read extracted root directory '{}': {error}",
            only_entry_path.display()
        ))
    })? {
        let child = child.map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to list extracted root directory '{}': {error}",
                only_entry_path.display()
            ))
        })?;
        let child_path = child.path();
        let child_name = child.file_name();
        let target_path = directory.join(child_name);
        fs::rename(&child_path, &target_path).map_err(|error| {
            FetchError::FsFailed(format!(
                "failed to normalize extracted root '{}' -> '{}': {error}",
                child_path.display(),
                target_path.display()
            ))
        })?;
    }

    fs::remove_dir(&only_entry_path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to remove extracted wrapper directory '{}': {error}",
            only_entry_path.display()
        ))
    })?;

    Ok(true)
}

fn select_archive_format(
    config: &FetchConfig,
    cached_blob: &Path,
    source_url: &str,
) -> FResult<ArchiveFormat> {
    if let Some(format) = &config.archive_format {
        return Ok(format.clone());
    }

    if let Some(format) = detect_archive_format_from_magic(cached_blob)? {
        return Ok(format);
    }

    if let Some(format) = detect_archive_format_from_url(source_url) {
        return Ok(format);
    }

    Err(FetchError::InvalidConfig(format!(
        "unable to detect archive format for url '{}'; set archive_format explicitly or use unpack = false",
        source_url
    )))
}

fn detect_archive_format_from_magic(path: &Path) -> FResult<Option<ArchiveFormat>> {
    let mut file = File::open(path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to open cached blob for archive detection '{}': {error}",
            path.display()
        ))
    })?;

    let mut header = [0_u8; 8];
    let bytes_read = file.read(&mut header).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to read cached blob for archive detection '{}': {error}",
            path.display()
        ))
    })?;
    let header = &header[..bytes_read];

    if header.len() >= 2 && header[0] == 0x1f && header[1] == 0x8b {
        return Ok(Some(ArchiveFormat::TarGz));
    }
    if header.len() >= 6
        && header[0] == 0xfd
        && header[1] == 0x37
        && header[2] == 0x7a
        && header[3] == 0x58
        && header[4] == 0x5a
        && header[5] == 0x00
    {
        return Ok(Some(ArchiveFormat::TarXz));
    }
    if header.len() >= 3 && header[0] == 0x42 && header[1] == 0x5a && header[2] == 0x68 {
        return Ok(Some(ArchiveFormat::TarBz2));
    }
    if header.len() >= 4
        && header[0] == 0x50
        && header[1] == 0x4b
        && matches!(header[2], 0x03 | 0x05 | 0x07)
        && matches!(header[3], 0x04 | 0x06 | 0x08)
    {
        return Ok(Some(ArchiveFormat::Zip));
    }

    Ok(None)
}

fn detect_archive_format_from_url(url: &str) -> Option<ArchiveFormat> {
    let url_lower = url.to_ascii_lowercase();
    if url_lower.ends_with(".tar.gz") || url_lower.ends_with(".tgz") {
        return Some(ArchiveFormat::TarGz);
    }
    if url_lower.ends_with(".tar.xz") {
        return Some(ArchiveFormat::TarXz);
    }
    if url_lower.ends_with(".tar.bz2")
        || url_lower.ends_with(".tbz2")
        || url_lower.ends_with(".tbz")
    {
        return Some(ArchiveFormat::TarBz2);
    }
    if url_lower.ends_with(".zip") {
        return Some(ArchiveFormat::Zip);
    }
    None
}

fn archive_format_name(format: &ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::TarGz => "tar-gz",
        ArchiveFormat::TarXz => "tar-xz",
        ArchiveFormat::TarBz2 => "tar-bz2",
        ArchiveFormat::Zip => "zip",
    }
}

fn ensure_dir(path: &Path, label: &str) -> FResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        FetchError::FsFailed(format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        ))
    })
}

fn map_fsutil_error(error: fsutil::FsUtilError) -> FetchError {
    FetchError::FsFailed(error.to_string())
}

fn map_error(error: FetchError) -> BuilderError {
    match error {
        FetchError::InvalidConfig(message) => BuilderError::InvalidRecipe(message),
        FetchError::NetworkFailed(message)
        | FetchError::ExtractFailed(message)
        | FetchError::FsFailed(message) => BuilderError::ExecutionFailed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::{BuildKey, Builder, ObjectHash, ResolvedInputValue};
    use sha2::Digest;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn build_context(root: &Path) -> BuildContext {
        BuildContext {
            workspace_root: root.to_path_buf(),
            builder_root: root.join(".mbuild").join("builder-state").join("fetch"),
            temp_root: root
                .join(".mbuild")
                .join("builder-state")
                .join("fetch")
                .join("tmp"),
        }
    }

    fn spawn_http_server(
        body: Vec<u8>,
        content_type: &'static str,
    ) -> Result<(String, thread::JoinHandle<()>), std::io::Error> {
        let listener = (0..10)
            .find_map(|attempt| match TcpListener::bind("127.0.0.1:0") {
                Ok(listener) => Some(Ok(listener)),
                Err(error)
                    if attempt < 9
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrInUse
                        ) =>
                {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
                Err(error) => Some(Err(error)),
            })
            .unwrap_or_else(|| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "failed to bind test HTTP listener",
                ))
            })?;
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/payload", addr);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            drain_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                body.len(),
                content_type
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        });
        Ok((url, handle))
    }

    fn drain_request(stream: &mut TcpStream) {
        let mut buf = [0u8; 1024];
        let mut request = Vec::new();
        loop {
            let read = stream.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
    }

    fn sample_input() -> ResolvedInputs {
        let mut inputs = ResolvedInputs::empty();
        inputs.insert(
            "unexpected",
            ResolvedInputValue::One(mbuild_core::ResolvedObject {
                object_hash:
                    "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                        .parse::<ObjectHash>()
                        .unwrap(),
                build_key:
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                        .parse::<BuildKey>()
                        .unwrap(),
                kind: "source-tree".to_string(),
                attrs: Map::new(),
                object_path: PathBuf::from("/tmp/input"),
            }),
        );
        inputs
    }

    fn tar_gz_with_wrapped_root() -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);

        let mut header = tar::Header::new_gnu();
        let body = b"hello archive\n";
        header.set_path("pkg-1.0/README.txt").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &body[..]).unwrap();

        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn fetch_builder_downloads_file_and_reports_attrs() {
        let temp = tempdir().unwrap();
        let payload = b"hello fetch\n".to_vec();
        let hash = format!("sha256:{}", bytes_to_hex(&Sha256::digest(&payload)));
        let (url, handle) = match spawn_http_server(payload.clone(), "application/octet-stream") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping fetch unit test because TCP bind is not permitted in this environment: {error}"
                );
                return;
            }
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };

        let mut cx = build_context(temp.path());
        let builder = FetchBuilder;
        let result = builder
            .build_typed(
                FetchConfig {
                    url: UrlField::One(url.clone()),
                    hash: hash.clone(),
                    unpack: false,
                    archive_format: None,
                    kind: None,
                },
                ResolvedInputs::empty(),
                &mut cx,
            )
            .unwrap();

        handle.join().unwrap();

        assert_eq!(result.kind, "fetched-file");
        assert_eq!(result.producer.builder, "fetch");
        assert_eq!(fs::read(&result.staged_path).unwrap(), payload);
        assert_eq!(result.attrs["source_url"], Value::String(url));
        assert_eq!(result.attrs["declared_hash"], Value::String(hash));
        assert_eq!(result.attrs["unpack"], Value::Bool(false));
    }

    #[test]
    fn fetch_builder_unpacks_tar_gz_and_sets_archive_attrs() {
        let temp = tempdir().unwrap();
        let payload = tar_gz_with_wrapped_root();
        let hash = format!("sha256:{}", bytes_to_hex(&Sha256::digest(&payload)));
        let (url, handle) = match spawn_http_server(payload, "application/gzip") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping fetch archive unit test because TCP bind is not permitted in this environment: {error}"
                );
                return;
            }
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };

        let mut cx = build_context(temp.path());
        let builder = FetchBuilder;
        let result = builder
            .build_typed(
                FetchConfig {
                    url: UrlField::One(url.clone()),
                    hash: hash.clone(),
                    unpack: true,
                    archive_format: None,
                    kind: None,
                },
                ResolvedInputs::empty(),
                &mut cx,
            )
            .unwrap();

        handle.join().unwrap();

        assert_eq!(result.kind, "source-tree");
        assert!(result.staged_path.is_dir());
        assert_eq!(
            fs::read_to_string(result.staged_path.join("README.txt")).unwrap(),
            "hello archive\n"
        );
        assert_eq!(result.attrs["source_url"], Value::String(url));
        assert_eq!(result.attrs["declared_hash"], Value::String(hash));
        assert_eq!(result.attrs["unpack"], Value::Bool(true));
        assert_eq!(
            result.attrs["archive_format"],
            Value::String("tar-gz".to_string())
        );
        assert_eq!(result.attrs["normalized_root"], Value::Bool(true));
        #[cfg(unix)]
        {
            let mode = fs::metadata(result.staged_path.join("README.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644);
        }
    }

    #[test]
    fn fetch_builder_reuses_cached_blob_without_second_server() {
        let temp = tempdir().unwrap();
        let payload = b"hello cached fetch\n".to_vec();
        let hash = format!("sha256:{}", bytes_to_hex(&Sha256::digest(&payload)));
        let (url, handle) = match spawn_http_server(payload.clone(), "application/octet-stream") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping fetch unit test because TCP bind is not permitted in this environment: {error}"
                );
                return;
            }
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };

        let builder = FetchBuilder;
        let mut first_cx = build_context(temp.path());
        let first = builder
            .build_typed(
                FetchConfig {
                    url: UrlField::One(url.clone()),
                    hash: hash.clone(),
                    unpack: false,
                    archive_format: None,
                    kind: None,
                },
                ResolvedInputs::empty(),
                &mut first_cx,
            )
            .unwrap();
        handle.join().unwrap();

        let mut second_cx = build_context(temp.path());
        let second = builder
            .build_typed(
                FetchConfig {
                    url: UrlField::One(url),
                    hash,
                    unpack: false,
                    archive_format: None,
                    kind: None,
                },
                ResolvedInputs::empty(),
                &mut second_cx,
            )
            .unwrap();

        assert_eq!(fs::read(&first.staged_path).unwrap(), payload);
        assert_eq!(fs::read(&second.staged_path).unwrap(), payload);
        let cache_blob = first_cx.builder_root.join("cache").join("sha256");
        assert_eq!(fs::read_dir(cache_blob).unwrap().count(), 1);
    }

    #[test]
    fn fetch_builder_rejects_non_empty_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let builder = FetchBuilder;
        let error = builder
            .build_typed(
                FetchConfig {
                    url: UrlField::One("https://example.invalid/archive.tar.gz".to_string()),
                    hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                        .to_string(),
                    unpack: false,
                    archive_format: None,
                    kind: None,
                },
                sample_input(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::ExecutionFailed(_)));
        assert!(error.to_string().contains("does not accept input objects"));
    }

    #[test]
    fn build_erased_rejects_unknown_config_field() {
        let temp = tempdir().unwrap();
        let mut cx = build_context(temp.path());
        let builder = FetchBuilder;

        let error = builder
            .build_erased(
                serde_json::json!({
                    "url": "https://example.invalid/archive.tar.gz",
                    "hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                    "extra": true
                }),
                ResolvedInputs::empty(),
                &mut cx,
            )
            .unwrap_err();

        assert!(matches!(error, BuilderError::InvalidRecipe(_)));
        assert!(error.to_string().contains("unknown field"));
    }
}
