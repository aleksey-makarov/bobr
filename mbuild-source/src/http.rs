use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use mbuild_core::{OriginContext, OriginHandler, OriginSpec, ParsedOrigin};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde_json::{Map, Value};
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

const REDIRECT_LIMIT: usize = 10;
const USER_AGENT: &str = "mbuild-source-http/0.1";

static HTTP_ORIGIN_SPEC: OriginSpec = OriginSpec { tag: "Http" };

#[derive(Debug)]
enum HttpOriginError {
    InvalidConfig(String),
    NetworkFailed(String),
    ExtractFailed(String),
    FsFailed(String),
}

impl HttpOriginError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::NetworkFailed(message)
            | Self::ExtractFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for HttpOriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type HResult<T> = Result<T, HttpOriginError>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArchiveFormat {
    TarGz,
    TarXz,
    TarBz2,
    Zip,
}

#[derive(Debug, Clone)]
enum UrlField {
    One(String),
    Many(Vec<String>),
}

impl UrlField {
    fn into_list(self) -> Vec<String> {
        match self {
            Self::One(url) => vec![url],
            Self::Many(urls) => urls,
        }
    }
}

#[derive(Debug)]
pub struct HttpOriginHandler;

#[derive(Debug, Clone)]
struct HttpOrigin {
    urls: Vec<String>,
    unpack: bool,
    archive_format: Option<ArchiveFormat>,
}

impl OriginHandler for HttpOriginHandler {
    fn spec(&self) -> &'static OriginSpec {
        &HTTP_ORIGIN_SPEC
    }

    fn parse(
        &self,
        mut object: Map<String, Value>,
        field_path: &str,
    ) -> Result<Box<dyn ParsedOrigin>, String> {
        let kind = take_string(&mut object, field_path, "tag")?;
        debug_assert_eq!(kind, "Http");
        let urls = take_url_field(&mut object, field_path, "url")?.into_list();
        if urls.is_empty() {
            return Err(format!("{field_path}.url: url list must not be empty"));
        }
        for url in &urls {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!(
                    "{field_path}.url: url '{url}' must start with http:// or https://"
                ));
            }
        }
        let unpack = take_optional_bool(&mut object, field_path, "unpack")?.unwrap_or(false);
        let archive_format =
            take_optional_archive_format(&mut object, field_path, "archive_format")?;
        if !object.is_empty() {
            return Err(format!(
                "{field_path}: unexpected fields: {}",
                object.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(Box::new(HttpOrigin {
            urls,
            unpack,
            archive_format,
        }))
    }
}

impl ParsedOrigin for HttpOrigin {
    fn spec(&self) -> &'static OriginSpec {
        &HTTP_ORIGIN_SPEC
    }

    fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String> {
        materialize_http_origin(cx.temp_root, self).map_err(|error| error.to_string())
    }

    fn clone_box(&self) -> Box<dyn ParsedOrigin> {
        Box::new(self.clone())
    }
}

fn materialize_http_origin(temp_root: &Path, origin: &HttpOrigin) -> HResult<PathBuf> {
    let client = Client::builder()
        .redirect(Policy::limited(REDIRECT_LIMIT))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|error| {
            HttpOriginError::NetworkFailed(format!("failed to create HTTP client: {error}"))
        })?;
    let downloaded_blob = download_first_success(temp_root, &client, &origin.urls)?;
    if !origin.unpack {
        return Ok(downloaded_blob);
    }

    let format = select_archive_format(
        origin.archive_format.as_ref(),
        &downloaded_blob,
        &origin.urls,
    )?;
    let staged_dir = temp_root.join("staged");
    recreate_empty_dir_force(&staged_dir)?;
    extract_archive(&downloaded_blob, format, &staged_dir)?;
    let _ = normalize_extracted_root(&staged_dir)?;
    Ok(staged_dir)
}

fn take_string(object: &mut Map<String, Value>, path: &str, field: &str) -> Result<String, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{path}.{field}: expected string"))
}

fn take_url_field(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<UrlField, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    match value {
        Value::String(url) => Ok(UrlField::One(url)),
        Value::Array(items) => {
            let mut urls = Vec::with_capacity(items.len());
            for item in items {
                let Value::String(url) = item else {
                    return Err(format!(
                        "{path}.{field}: expected string or array of strings"
                    ));
                };
                urls.push(url);
            }
            Ok(UrlField::Many(urls))
        }
        _ => Err(format!(
            "{path}.{field}: expected string or array of strings"
        )),
    }
}

fn take_optional_bool(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = object.remove(field) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("{path}.{field}: expected boolean"))
}

fn take_optional_archive_format(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<Option<ArchiveFormat>, String> {
    let Some(value) = object.remove(field) else {
        return Ok(None);
    };
    let Some(value) = value.as_str() else {
        return Err(format!("{path}.{field}: expected string"));
    };
    match value {
        "tar-gz" => Ok(Some(ArchiveFormat::TarGz)),
        "tar-xz" => Ok(Some(ArchiveFormat::TarXz)),
        "tar-bz2" => Ok(Some(ArchiveFormat::TarBz2)),
        "zip" => Ok(Some(ArchiveFormat::Zip)),
        _ => Err(format!(
            "{path}.{field}: unsupported archive format '{value}'"
        )),
    }
}

fn download_first_success(temp_root: &Path, client: &Client, urls: &[String]) -> HResult<PathBuf> {
    let download_path = temp_root.join("download.blob");
    if download_path.exists() {
        fs::remove_file(&download_path).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to remove stale download '{}': {error}",
                download_path.display()
            ))
        })?;
    }

    let mut failures = Vec::new();
    for url in urls {
        if let Err(error) = download_to_file(client, url, &download_path) {
            failures.push(format!("{url}: {error}"));
            let _ = fs::remove_file(&download_path);
            continue;
        }
        return Ok(download_path);
    }

    Err(HttpOriginError::NetworkFailed(format!(
        "all download URLs failed:\n  - {}",
        failures.join("\n  - ")
    )))
}

fn download_to_file(client: &Client, url: &str, destination: &Path) -> HResult<()> {
    let mut response = client.get(url).send().map_err(|error| {
        HttpOriginError::NetworkFailed(format!("failed to download '{url}': {error}"))
    })?;
    if !response.status().is_success() {
        return Err(HttpOriginError::NetworkFailed(format!(
            "failed to download '{url}': HTTP {}",
            response.status()
        )));
    }

    let mut file = File::create(destination).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to create temporary download file '{}': {error}",
            destination.display()
        ))
    })?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read_bytes = response.read(&mut buffer).map_err(|error| {
            HttpOriginError::NetworkFailed(format!("failed to read HTTP response body: {error}"))
        })?;
        if read_bytes == 0 {
            break;
        }
        file.write_all(&buffer[..read_bytes]).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to write temporary download file '{}': {error}",
                destination.display()
            ))
        })?;
    }
    Ok(())
}

fn select_archive_format(
    explicit_format: Option<&ArchiveFormat>,
    downloaded_blob: &Path,
    urls: &[String],
) -> HResult<ArchiveFormat> {
    if let Some(format) = explicit_format {
        return Ok(format.clone());
    }
    if let Some(format) = detect_archive_format_from_magic(downloaded_blob)? {
        return Ok(format);
    }
    for url in urls {
        if let Some(format) = detect_archive_format_from_url(url) {
            return Ok(format);
        }
    }
    Err(HttpOriginError::InvalidConfig(format!(
        "unable to detect archive format for URLs {}; set archive_format explicitly or use unpack = false",
        urls.join(", ")
    )))
}

fn detect_archive_format_from_magic(path: &Path) -> HResult<Option<ArchiveFormat>> {
    let mut file = File::open(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to open cached blob for archive detection '{}': {error}",
            path.display()
        ))
    })?;
    let mut header = [0_u8; 8];
    let bytes_read = file.read(&mut header).map_err(|error| {
        HttpOriginError::FsFailed(format!(
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

fn extract_archive(archive_path: &Path, format: ArchiveFormat, destination: &Path) -> HResult<()> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, destination),
        ArchiveFormat::TarXz => extract_tar_xz(archive_path, destination),
        ArchiveFormat::TarBz2 => extract_tar_bz2(archive_path, destination),
        ArchiveFormat::Zip => extract_zip(archive_path, destination),
    }
}

fn extract_tar_gz(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open tar.gz archive '{}': {error}",
            archive_path.display()
        ))
    })?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn extract_tar_xz(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open tar.xz archive '{}': {error}",
            archive_path.display()
        ))
    })?;
    let decoder = XzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn extract_tar_bz2(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open tar.bz2 archive '{}': {error}",
            archive_path.display()
        ))
    })?;
    let decoder = BzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    unpack_tar_safely(&mut archive, destination)
}

fn unpack_tar_safely<R: Read>(archive: &mut Archive<R>, destination: &Path) -> HResult<()> {
    let entries = archive.entries().map_err(|error| {
        HttpOriginError::ExtractFailed(format!("failed to read tar archive entries: {error}"))
    })?;

    for entry_result in entries {
        let mut entry = entry_result.map_err(|error| {
            HttpOriginError::ExtractFailed(format!("failed to parse tar entry: {error}"))
        })?;

        entry.unpack_in(destination).map_err(|error| {
            HttpOriginError::ExtractFailed(format!(
                "failed to extract tar entry into '{}': {error}",
                destination.display()
            ))
        })?;
    }

    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> HResult<()> {
    let file = File::open(archive_path).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open zip archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    let mut zip = ZipArchive::new(file).map_err(|error| {
        HttpOriginError::ExtractFailed(format!(
            "failed to open zip archive '{}': {error}",
            archive_path.display()
        ))
    })?;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| {
            HttpOriginError::ExtractFailed(format!("failed to read zip entry #{index}: {error}"))
        })?;

        let enclosed = entry.enclosed_name().ok_or_else(|| {
            HttpOriginError::ExtractFailed(format!(
                "zip entry '{}' has invalid or unsafe path",
                entry.name()
            ))
        })?;

        let target_path = destination.join(enclosed);
        if !target_path
            .components()
            .all(|component| !matches!(component, Component::ParentDir | Component::RootDir))
        {
            return Err(HttpOriginError::ExtractFailed(format!(
                "zip entry '{}' resolves to unsafe path",
                entry.name()
            )));
        }

        if entry.is_dir() {
            fs::create_dir_all(&target_path).map_err(|error| {
                HttpOriginError::ExtractFailed(format!(
                    "failed to create directory '{}': {error}",
                    target_path.display()
                ))
            })?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                HttpOriginError::ExtractFailed(format!(
                    "failed to create parent directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }

        let mut out = File::create(&target_path).map_err(|error| {
            HttpOriginError::ExtractFailed(format!(
                "failed to create file '{}': {error}",
                target_path.display()
            ))
        })?;

        std::io::copy(&mut entry, &mut out).map_err(|error| {
            HttpOriginError::ExtractFailed(format!(
                "failed to extract zip entry '{}' to '{}': {error}",
                entry.name(),
                target_path.display()
            ))
        })?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            fs::set_permissions(&target_path, fs::Permissions::from_mode(mode)).map_err(
                |error| {
                    HttpOriginError::ExtractFailed(format!(
                        "failed to set permissions on '{}': {error}",
                        target_path.display()
                    ))
                },
            )?;
        }
    }

    Ok(())
}

fn normalize_extracted_root(directory: &Path) -> HResult<bool> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to read extracted directory '{}': {error}",
                directory.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            HttpOriginError::FsFailed(format!(
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
        HttpOriginError::FsFailed(format!(
            "failed to inspect extracted entry '{}': {error}",
            only_entry_path.display()
        ))
    })?;
    if !only_entry_file_type.is_dir() {
        return Ok(false);
    }

    for child in fs::read_dir(&only_entry_path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to read extracted root directory '{}': {error}",
            only_entry_path.display()
        ))
    })? {
        let child = child.map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to list extracted root directory '{}': {error}",
                only_entry_path.display()
            ))
        })?;
        let child_path = child.path();
        let target_path = directory.join(child.file_name());
        fs::rename(&child_path, &target_path).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to normalize extracted root '{}' -> '{}': {error}",
                child_path.display(),
                target_path.display()
            ))
        })?;
    }

    fs::remove_dir(&only_entry_path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to remove extracted wrapper directory '{}': {error}",
            only_entry_path.display()
        ))
    })?;

    Ok(true)
}

fn recreate_empty_dir_force(path: &Path) -> HResult<()> {
    if fs::symlink_metadata(path).is_ok() {
        if path.is_dir() && !path.is_symlink() {
            remove_dir_force(path)?;
        } else {
            fs::remove_file(path).map_err(|error| {
                HttpOriginError::FsFailed(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

fn remove_dir_force(path: &Path) -> HResult<()> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    make_tree_writable(path)?;
    fs::remove_dir_all(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to remove directory '{}': {error}",
            path.display()
        ))
    })
}

fn make_tree_writable(path: &Path) -> HResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        HttpOriginError::FsFailed(format!(
            "failed to inspect path '{}': {error}",
            path.display()
        ))
    })?;

    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_dir() {
        let mode = metadata.permissions().mode();
        let desired = mode | 0o700;
        if desired != mode {
            fs::set_permissions(path, fs::Permissions::from_mode(desired)).map_err(|error| {
                HttpOriginError::FsFailed(format!(
                    "failed to adjust permissions for '{}': {error}",
                    path.display()
                ))
            })?;
        }

        for entry in fs::read_dir(path).map_err(|error| {
            HttpOriginError::FsFailed(format!(
                "failed to read directory '{}': {error}",
                path.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                HttpOriginError::FsFailed(format!(
                    "failed to read directory entry in '{}': {error}",
                    path.display()
                ))
            })?;
            make_tree_writable(&entry.path())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use std::io::{Cursor, Read};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn parse_origin(value: Value) -> Result<Box<dyn ParsedOrigin>, String> {
        HttpOriginHandler.parse(value.as_object().unwrap().clone(), "$.origin")
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

    fn spawn_fallback_server(
        ok_body: Vec<u8>,
        content_type: &'static str,
    ) -> Result<((String, String), thread::JoinHandle<()>), std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr().unwrap();
        let bad_url = format!("http://{}/bad", addr);
        let good_url = format!("http://{}/good", addr);
        let handle = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("GET /bad ") {
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                        ok_body.len(),
                        content_type
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.write_all(&ok_body).unwrap();
                }
                stream.flush().unwrap();
            }
        });
        Ok(((bad_url, good_url), handle))
    }

    fn drain_request(stream: &mut TcpStream) {
        let _ = read_request(stream);
    }

    fn read_request(stream: &mut TcpStream) -> String {
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
        String::from_utf8_lossy(&request).into_owned()
    }

    fn tar_gz_with_wrapped_root() -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let body = b"hello archive\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg-1.0/README.txt").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &body[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn parses_valid_http_origin() {
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": ["https://example.invalid/a.tar.gz", "https://example.invalid/b.tar.gz"],
            "unpack": false,
            "archive_format": "zip"
        }))
        .unwrap();
        assert_eq!(origin.spec().tag, "Http");
    }

    #[test]
    fn rejects_invalid_url_shape() {
        let error = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": [1, 2]
        }))
        .unwrap_err();
        assert!(
            error.contains("expected string or array of strings"),
            "{error}"
        );
    }

    #[test]
    fn rejects_non_http_urls() {
        let error = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": "ftp://example.invalid/source.tar.gz"
        }))
        .unwrap_err();
        assert!(
            error.contains("must start with http:// or https://"),
            "{error}"
        );
    }

    #[test]
    fn fallback_url_list_works_in_order() {
        let temp = tempdir().unwrap();
        let payload = b"hello fallback\n".to_vec();
        let ((bad_url, good_url), handle) =
            spawn_fallback_server(payload.clone(), "application/octet-stream").unwrap();
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": [bad_url, good_url],
            "unpack": false
        }))
        .unwrap();
        let staged = origin
            .materialize(&OriginContext {
                temp_root: temp.path(),
            })
            .unwrap();
        handle.join().unwrap();
        assert_eq!(fs::read(staged).unwrap(), payload);
    }

    #[test]
    fn unpack_false_yields_file_object() {
        let temp = tempdir().unwrap();
        let payload = b"hello file\n".to_vec();
        let (url, handle) = match spawn_http_server(payload.clone(), "application/octet-stream") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url,
            "unpack": false
        }))
        .unwrap();
        let staged = origin
            .materialize(&OriginContext {
                temp_root: temp.path(),
            })
            .unwrap();
        handle.join().unwrap();
        assert!(staged.is_file());
        assert_eq!(fs::read(staged).unwrap(), payload);
    }

    #[test]
    fn omitted_unpack_yields_file_object() {
        let temp = tempdir().unwrap();
        let payload = b"hello default file\n".to_vec();
        let (url, handle) = match spawn_http_server(payload.clone(), "application/octet-stream") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url
        }))
        .unwrap();
        let staged = origin
            .materialize(&OriginContext {
                temp_root: temp.path(),
            })
            .unwrap();
        handle.join().unwrap();
        assert!(staged.is_file());
        assert_eq!(fs::read(staged).unwrap(), payload);
    }

    #[test]
    fn unpack_true_yields_unpacked_tree_object() {
        let temp = tempdir().unwrap();
        let payload = tar_gz_with_wrapped_root();
        let (url, handle) = match spawn_http_server(payload, "application/gzip") {
            Ok(server) => server,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to start test HTTP server: {error}"),
        };
        let origin = parse_origin(serde_json::json!({
            "tag": "Http",
            "url": url,
            "unpack": true
        }))
        .unwrap();
        let staged = origin
            .materialize(&OriginContext {
                temp_root: temp.path(),
            })
            .unwrap();
        handle.join().unwrap();
        assert!(staged.is_dir());
        assert_eq!(
            fs::read_to_string(staged.join("README.txt")).unwrap(),
            "hello archive\n"
        );
    }

    #[test]
    fn magic_based_archive_detection_works() {
        let temp = tempdir().unwrap();
        let payload = tar_gz_with_wrapped_root();
        let path = temp.path().join("payload.bin");
        fs::write(&path, payload).unwrap();
        let detected = detect_archive_format_from_magic(&path).unwrap();
        assert_eq!(detected, Some(ArchiveFormat::TarGz));
    }

    #[test]
    fn url_suffix_fallback_detection_works() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("payload.bin");
        fs::write(&path, b"not-an-archive").unwrap();
        let detected = select_archive_format(
            None,
            &path,
            &[String::from("https://example.invalid/source.tar.gz")],
        )
        .unwrap();
        assert_eq!(detected, ArchiveFormat::TarGz);
    }

    #[test]
    fn explicit_archive_format_override_works() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("payload.bin");
        fs::write(&path, b"not-an-archive").unwrap();
        let detected = select_archive_format(
            Some(&ArchiveFormat::Zip),
            &path,
            &[String::from("https://example.invalid/source.tar.gz")],
        )
        .unwrap();
        assert_eq!(detected, ArchiveFormat::Zip);
    }

    #[test]
    fn unpacked_tree_matches_tar_hashing_model() {
        let payload = tar_gz_with_wrapped_root();
        let tree_hash = fsobj_hash::hash_tar_reader(GzDecoder::new(Cursor::new(&payload))).unwrap();
        assert_eq!(tree_hash.to_string().len(), 64);
    }
}
