//! Streaming repository object and filesystem-file envelopes.

use crate::{
    MAX_DECODED_CONTENT_BYTES, MAX_ENCODED_CONTENT_BYTES, MAX_ZSTD_WINDOW_BYTES, RepositoryError,
    extract_directory_tar, write_directory_tar,
};
use bobr_core::{CANONICAL_TIMESTAMP, ObjectHash};
use bobr_store::fs_tree::{FsFileHash, hash_fs_file_parts};
use fsobj_hash::{hash_file_node, hash_path};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Repository payload compression selected by an immutable envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Payload bytes are stored directly.
    Identity,
    /// Payload bytes form one Zstandard frame.
    Zstd,
}

/// Kind of an ordinary object admitted after decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A regular file with its hash-relevant executable state.
    File {
        /// Whether at least one executable mode bit is logically set.
        executable: bool,
    },
    /// A directory transported using the repository tar profile.
    Directory,
}

/// Authenticated logical metadata of an imported filesystem file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsFileMetadata {
    /// Logical numeric owner.
    pub uid: u32,
    /// Logical numeric group.
    pub gid: u32,
    /// Complete logical mode in `0..=0o7777`.
    pub mode: u32,
    /// Exact byte length after decompression.
    pub decoded_size: u64,
}

/// Encodes one ordinary object to an immutable repository representation.
pub fn encode_object(
    source: &Path,
    compression: Compression,
    output: &Path,
) -> Result<ObjectHash, RepositoryError> {
    let metadata = fs::symlink_metadata(source)?;
    let parent = output_parent(output)?;
    let mut payload = tempfile::NamedTempFile::new_in(parent)?;
    let (kind, decoded_size, object_hash) = if metadata.file_type().is_file() {
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let (size, content_hash) = write_encoded_file(source, compression, payload.as_file_mut())?;
        (
            ObjectKind::File { executable },
            size,
            hash_file_node(executable, size, content_hash),
        )
    } else if metadata.file_type().is_dir() {
        let object_hash = hash_path(source)
            .map_err(|error| RepositoryError::new(format!("failed to hash directory: {error}")))?;
        let decoded_size = write_encoded_tar(source, compression, payload.as_file_mut())?;
        (ObjectKind::Directory, decoded_size, object_hash)
    } else {
        return Err(RepositoryError::new(
            "ordinary repository object must be a regular file or directory",
        ));
    };
    payload.as_file_mut().flush()?;
    let encoded_size = payload.as_file().metadata()?.len();
    check_content_sizes(encoded_size, decoded_size)?;
    let mut out = fs::File::create(output)?;
    write_object_prefix(&mut out, kind, compression, decoded_size, encoded_size)?;
    payload.as_file_mut().seek(SeekFrom::Start(0))?;
    io::copy(payload.as_file_mut(), &mut out)?;
    out.sync_all()?;
    Ok(object_hash)
}

/// Decodes, verifies, normalizes, and atomically places one ordinary object.
pub fn decode_object(
    encoded: &Path,
    expected: ObjectHash,
    destination: &Path,
) -> Result<ObjectKind, RepositoryError> {
    require_absent(destination)?;
    let envelope = parse_object_envelope(encoded)?;
    let parent = output_parent(destination)?;
    match envelope.kind {
        ObjectKind::File { executable } => {
            let mut staging = tempfile::NamedTempFile::new_in(parent)?;
            let (size, content_hash) = decode_payload(encoded, &envelope, staging.as_file_mut())?;
            let actual = hash_file_node(executable, size, content_hash);
            if actual != expected {
                return Err(hash_mismatch("object", expected, actual));
            }
            let mode = if executable { 0o755 } else { 0o644 };
            fs::set_permissions(staging.path(), fs::Permissions::from_mode(mode))?;
            set_canonical_times(staging.path())?;
            staging.as_file().sync_all()?;
            staging
                .persist_noclobber(destination)
                .map_err(|error| error.error)?;
        }
        ObjectKind::Directory => {
            let staging_parent = tempfile::Builder::new()
                .prefix(".bobr-repo-object-")
                .tempdir_in(parent)?;
            let staging = staging_parent.path().join("root");
            let decoded = open_decoded_payload(encoded, &envelope)?;
            let mut decoded = ExactDecodedPayload::new(decoded, envelope.decoded_size);
            let decoded_size = extract_directory_tar(&mut decoded, &staging)?;
            decoded.finish()?;
            require_decoded_size(decoded_size, envelope.decoded_size)?;
            let actual = hash_path(&staging).map_err(|error| {
                RepositoryError::new(format!("failed to hash decoded tree: {error}"))
            })?;
            if actual != expected {
                return Err(hash_mismatch("object", expected, actual));
            }
            fs::rename(&staging, destination)?;
        }
    }
    Ok(envelope.kind)
}

/// Encodes one canonical fs-file repository representation.
pub fn encode_fs_file(
    source: &Path,
    compression: Compression,
    output: &Path,
) -> Result<FsFileHash, RepositoryError> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_file() {
        return Err(RepositoryError::new("fs-file source is not a regular file"));
    }
    let mode = metadata.permissions().mode() & 0o7777;
    let parent = output_parent(output)?;
    let mut payload = tempfile::NamedTempFile::new_in(parent)?;
    let (decoded_size, content_hash) =
        write_encoded_file(source, compression, payload.as_file_mut())?;
    let hash = hash_fs_file_parts(
        metadata.uid(),
        metadata.gid(),
        mode,
        decoded_size,
        content_hash,
    )
    .map_err(|error| RepositoryError::new(error.to_string()))?;
    let encoded_size = payload.as_file().metadata()?.len();
    check_content_sizes(encoded_size, decoded_size)?;
    let mut out = fs::File::create(output)?;
    write_fs_file_prefix(
        &mut out,
        FsFileMetadata {
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode,
            decoded_size,
        },
        compression,
        encoded_size,
    )?;
    payload.as_file_mut().seek(SeekFrom::Start(0))?;
    io::copy(payload.as_file_mut(), &mut out)?;
    out.sync_all()?;
    Ok(hash)
}

/// Decodes, verifies, normalizes, and atomically places one filesystem file.
pub fn decode_fs_file(
    encoded: &Path,
    expected: FsFileHash,
    destination: &Path,
) -> Result<FsFileMetadata, RepositoryError> {
    require_absent(destination)?;
    let envelope = parse_fs_file_envelope(encoded)?;
    let parent = output_parent(destination)?;
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    let (size, content_hash) = decode_payload(encoded, &envelope.common, staging.as_file_mut())?;
    let actual = hash_fs_file_parts(
        envelope.metadata.uid,
        envelope.metadata.gid,
        envelope.metadata.mode,
        size,
        content_hash,
    )
    .map_err(|error| RepositoryError::new(error.to_string()))?;
    if actual != expected {
        return Err(RepositoryError::new(format!(
            "fs-file hash mismatch: expected '{expected}', got '{actual}'"
        )));
    }
    set_canonical_times(staging.path())?;
    chown(staging.path(), envelope.metadata.uid, envelope.metadata.gid)?;
    fs::set_permissions(
        staging.path(),
        fs::Permissions::from_mode(envelope.metadata.mode),
    )?;
    let normalized = fs::symlink_metadata(staging.path())?;
    if normalized.uid() != envelope.metadata.uid
        || normalized.gid() != envelope.metadata.gid
        || normalized.permissions().mode() & 0o7777 != envelope.metadata.mode
        || hash_fs_file_parts(
            normalized.uid(),
            normalized.gid(),
            normalized.permissions().mode() & 0o7777,
            normalized.size(),
            sha256_file(staging.path())?,
        )
        .map_err(|error| RepositoryError::new(error.to_string()))?
            != expected
    {
        return Err(RepositoryError::new(
            "normalized fs-file does not retain its declared identity",
        ));
    }
    staging.as_file().sync_all()?;
    staging
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    Ok(envelope.metadata)
}

#[derive(Debug)]
struct CommonEnvelope {
    compression: Compression,
    decoded_size: u64,
    payload_offset: u64,
    encoded_size: u64,
}

#[derive(Debug)]
struct ObjectEnvelope {
    kind: ObjectKind,
    common: CommonEnvelope,
}

impl std::ops::Deref for ObjectEnvelope {
    type Target = CommonEnvelope;

    fn deref(&self) -> &Self::Target {
        &self.common
    }
}

#[derive(Debug)]
struct ParsedFsFileEnvelope {
    metadata: FsFileMetadata,
    common: CommonEnvelope,
}

fn parse_object_envelope(path: &Path) -> Result<ObjectEnvelope, RepositoryError> {
    check_encoded_file(path)?;
    let mut file = fs::File::open(path)?;
    let mut decoder = StreamDecoder::new(&mut file);
    require_length(decoder.array()?, 2, "object envelope array")?;
    let fields = decoder.map()?;
    require_text(&mut decoder, "kind")?;
    let kind_text = decoder.text()?;
    let kind = match kind_text.as_str() {
        "file" => ObjectKind::File { executable: false },
        "directory" => ObjectKind::Directory,
        _ => return Err(RepositoryError::new("unsupported ordinary-object kind")),
    };
    let kind = match kind {
        ObjectKind::File { .. } => {
            require_length(fields, 4, "file metadata map")?;
            require_text(&mut decoder, "executable")?;
            ObjectKind::File {
                executable: decoder.boolean()?,
            }
        }
        ObjectKind::Directory => {
            require_length(fields, 4, "directory metadata map")?;
            require_text(&mut decoder, "archive")?;
            if decoder.text()? != "tar" {
                return Err(RepositoryError::new("unsupported directory archive format"));
            }
            ObjectKind::Directory
        }
    };
    require_text(&mut decoder, "compression")?;
    let compression = parse_compression(&decoder.text()?)?;
    require_text(&mut decoder, "decoded_size")?;
    let decoded_size = decoder.uint()?;
    let encoded_size = decoder.bytes_length()?;
    let payload_offset = decoder.position()?;
    validate_payload_extent(&file, payload_offset, encoded_size, decoded_size)?;
    Ok(ObjectEnvelope {
        kind,
        common: CommonEnvelope {
            compression,
            decoded_size,
            payload_offset,
            encoded_size,
        },
    })
}

fn parse_fs_file_envelope(path: &Path) -> Result<ParsedFsFileEnvelope, RepositoryError> {
    check_encoded_file(path)?;
    let mut file = fs::File::open(path)?;
    let mut decoder = StreamDecoder::new(&mut file);
    require_length(decoder.array()?, 2, "fs-file envelope array")?;
    require_length(decoder.map()?, 5, "fs-file metadata map")?;
    require_text(&mut decoder, "gid")?;
    let gid = require_u32(decoder.uint()?, "gid")?;
    require_text(&mut decoder, "uid")?;
    let uid = require_u32(decoder.uint()?, "uid")?;
    require_text(&mut decoder, "mode")?;
    let mode = require_u32(decoder.uint()?, "mode")?;
    if mode > 0o7777 {
        return Err(RepositoryError::new("fs-file mode exceeds 0o7777"));
    }
    require_text(&mut decoder, "compression")?;
    let compression = parse_compression(&decoder.text()?)?;
    require_text(&mut decoder, "decoded_size")?;
    let decoded_size = decoder.uint()?;
    let encoded_size = decoder.bytes_length()?;
    let payload_offset = decoder.position()?;
    validate_payload_extent(&file, payload_offset, encoded_size, decoded_size)?;
    Ok(ParsedFsFileEnvelope {
        metadata: FsFileMetadata {
            uid,
            gid,
            mode,
            decoded_size,
        },
        common: CommonEnvelope {
            compression,
            decoded_size,
            payload_offset,
            encoded_size,
        },
    })
}

fn write_object_prefix(
    output: &mut impl Write,
    kind: ObjectKind,
    compression: Compression,
    decoded_size: u64,
    encoded_size: u64,
) -> Result<(), RepositoryError> {
    write_head(output, 4, 2)?;
    write_head(output, 5, 4)?;
    write_text(output, "kind")?;
    match kind {
        ObjectKind::File { executable } => {
            write_text(output, "file")?;
            write_text(output, "executable")?;
            output.write_all(&[if executable { 0xf5 } else { 0xf4 }])?;
        }
        ObjectKind::Directory => {
            write_text(output, "directory")?;
            write_text(output, "archive")?;
            write_text(output, "tar")?;
        }
    }
    write_text(output, "compression")?;
    write_text(output, compression_name(compression))?;
    write_text(output, "decoded_size")?;
    write_head(output, 0, decoded_size)?;
    write_head(output, 2, encoded_size)?;
    Ok(())
}

fn write_fs_file_prefix(
    output: &mut impl Write,
    metadata: FsFileMetadata,
    compression: Compression,
    encoded_size: u64,
) -> Result<(), RepositoryError> {
    write_head(output, 4, 2)?;
    write_head(output, 5, 5)?;
    write_text(output, "gid")?;
    write_head(output, 0, metadata.gid as u64)?;
    write_text(output, "uid")?;
    write_head(output, 0, metadata.uid as u64)?;
    write_text(output, "mode")?;
    write_head(output, 0, metadata.mode as u64)?;
    write_text(output, "compression")?;
    write_text(output, compression_name(compression))?;
    write_text(output, "decoded_size")?;
    write_head(output, 0, metadata.decoded_size)?;
    write_head(output, 2, encoded_size)?;
    Ok(())
}

fn write_encoded_file(
    source: &Path,
    compression: Compression,
    output: &mut fs::File,
) -> Result<(u64, [u8; 32]), RepositoryError> {
    let mut source = fs::File::open(source)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    match compression {
        Compression::Identity => copy_hashing(&mut source, output, &mut hasher, &mut size)?,
        Compression::Zstd => {
            let mut encoder = zstd::stream::write::Encoder::new(output, 9)?;
            copy_hashing(&mut source, &mut encoder, &mut hasher, &mut size)?;
            encoder.finish()?;
        }
    }
    Ok((size, hasher.finalize().into()))
}

fn write_encoded_tar(
    source: &Path,
    compression: Compression,
    output: &mut fs::File,
) -> Result<u64, RepositoryError> {
    match compression {
        Compression::Identity => write_directory_tar(source, output),
        Compression::Zstd => {
            let mut encoder = zstd::stream::write::Encoder::new(output, 9)?;
            let size = write_directory_tar(source, &mut encoder)?;
            encoder.finish()?;
            Ok(size)
        }
    }
}

fn copy_hashing(
    input: &mut impl Read,
    output: &mut impl Write,
    hasher: &mut Sha256,
    size: &mut u64,
) -> Result<(), RepositoryError> {
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        *size = size
            .checked_add(count as u64)
            .ok_or_else(|| RepositoryError::new("decoded payload size overflow"))?;
        if *size > MAX_DECODED_CONTENT_BYTES {
            return Err(RepositoryError::new("decoded payload exceeds format limit"));
        }
    }
}

fn sha256_file(path: &Path) -> Result<[u8; 32], RepositoryError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..count]);
    }
}

fn decode_payload(
    encoded: &Path,
    envelope: &CommonEnvelope,
    output: &mut impl Write,
) -> Result<(u64, [u8; 32]), RepositoryError> {
    let mut decoded = open_decoded_payload(encoded, envelope)?;
    let mut hasher = Sha256::new();
    let mut size = 0;
    copy_hashing_exact(
        &mut decoded,
        output,
        &mut hasher,
        &mut size,
        envelope.decoded_size,
    )?;
    decoded.finish()?;
    require_decoded_size(size, envelope.decoded_size)?;
    Ok((size, hasher.finalize().into()))
}

fn copy_hashing_exact(
    input: &mut impl Read,
    output: &mut impl Write,
    hasher: &mut Sha256,
    size: &mut u64,
    expected: u64,
) -> Result<(), RepositoryError> {
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        *size = size
            .checked_add(count as u64)
            .ok_or_else(|| RepositoryError::new("decoded payload size overflow"))?;
        if *size > expected {
            return Err(RepositoryError::new(
                "decoded payload is larger than decoded_size",
            ));
        }
        output.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
    }
}

enum DecodedPayload {
    Identity(io::Take<fs::File>),
    Zstd(zstd::stream::read::Decoder<'static, BufReader<io::Take<fs::File>>>),
}

impl Read for DecodedPayload {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Identity(reader) => reader.read(buffer),
            Self::Zstd(reader) => reader.read(buffer),
        }
    }
}

impl DecodedPayload {
    fn finish(self) -> Result<(), RepositoryError> {
        match self {
            Self::Identity(reader) => {
                if reader.limit() != 0 {
                    return Err(RepositoryError::new("truncated identity payload"));
                }
            }
            Self::Zstd(reader) => {
                let buffered = reader.finish();
                if !buffered.buffer().is_empty() || buffered.get_ref().limit() != 0 {
                    return Err(RepositoryError::new(
                        "zstd payload contains a trailing frame or data",
                    ));
                }
            }
        }
        Ok(())
    }
}

struct ExactDecodedPayload {
    inner: DecodedPayload,
    remaining: u64,
}

impl ExactDecodedPayload {
    fn new(inner: DecodedPayload, expected: u64) -> Self {
        Self {
            inner,
            remaining: expected,
        }
    }

    fn finish(self) -> Result<(), RepositoryError> {
        if self.remaining != 0 {
            return Err(RepositoryError::new(
                "decoded payload is shorter than decoded_size",
            ));
        }
        self.inner.finish()
    }
}

impl Read for ExactDecodedPayload {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            let mut extra = [0];
            return match self.inner.read(&mut extra)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "decoded payload is larger than decoded_size",
                )),
            };
        }
        let limit = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

fn open_decoded_payload(
    encoded: &Path,
    envelope: &CommonEnvelope,
) -> Result<DecodedPayload, RepositoryError> {
    let mut file = fs::File::open(encoded)?;
    file.seek(SeekFrom::Start(envelope.payload_offset))?;
    let reader = file.take(envelope.encoded_size);
    match envelope.compression {
        Compression::Identity => Ok(DecodedPayload::Identity(reader)),
        Compression::Zstd => {
            let mut reader = reader;
            let mut magic = [0u8; 4];
            reader.read_exact(&mut magic)?;
            if magic != [0x28, 0xb5, 0x2f, 0xfd] {
                return Err(RepositoryError::new(
                    "zstd payload does not begin with one standard frame",
                ));
            }
            reader
                .get_mut()
                .seek(SeekFrom::Start(envelope.payload_offset))?;
            reader.set_limit(envelope.encoded_size);
            let mut decoder = zstd::stream::read::Decoder::new(reader)?.single_frame();
            decoder.window_log_max(MAX_ZSTD_WINDOW_BYTES.ilog2())?;
            Ok(DecodedPayload::Zstd(decoder))
        }
    }
}

fn validate_payload_extent(
    file: &fs::File,
    payload_offset: u64,
    encoded_size: u64,
    decoded_size: u64,
) -> Result<(), RepositoryError> {
    check_content_sizes(encoded_size, decoded_size)?;
    let expected_size = payload_offset
        .checked_add(encoded_size)
        .ok_or_else(|| RepositoryError::new("encoded envelope size overflow"))?;
    if file.metadata()?.len() != expected_size {
        return Err(RepositoryError::new(
            "encoded envelope is truncated or contains trailing bytes",
        ));
    }
    Ok(())
}

fn check_encoded_file(path: &Path) -> Result<(), RepositoryError> {
    if fs::metadata(path)?.len() > MAX_ENCODED_CONTENT_BYTES {
        return Err(RepositoryError::new("encoded content exceeds format limit"));
    }
    Ok(())
}

fn check_content_sizes(encoded: u64, decoded: u64) -> Result<(), RepositoryError> {
    if encoded > MAX_ENCODED_CONTENT_BYTES || decoded > MAX_DECODED_CONTENT_BYTES {
        return Err(RepositoryError::new(
            "repository content exceeds format limits",
        ));
    }
    Ok(())
}

fn require_decoded_size(actual: u64, expected: u64) -> Result<(), RepositoryError> {
    if actual == expected {
        Ok(())
    } else {
        Err(RepositoryError::new(format!(
            "decoded payload size mismatch: expected {expected}, got {actual}"
        )))
    }
}

fn output_parent(path: &Path) -> Result<&Path, RepositoryError> {
    path.parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| RepositoryError::new("output parent is not an existing directory"))
}

fn require_absent(path: &Path) -> Result<(), RepositoryError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(RepositoryError::new("decode destination already exists")),
        Err(error) => Err(error.into()),
    }
}

fn hash_mismatch(kind: &str, expected: ObjectHash, actual: ObjectHash) -> RepositoryError {
    RepositoryError::new(format!(
        "{kind} hash mismatch: expected '{expected}', got '{actual}'"
    ))
}

fn compression_name(compression: Compression) -> &'static str {
    match compression {
        Compression::Identity => "identity",
        Compression::Zstd => "zstd",
    }
}

fn parse_compression(value: &str) -> Result<Compression, RepositoryError> {
    match value {
        "identity" => Ok(Compression::Identity),
        "zstd" => Ok(Compression::Zstd),
        _ => Err(RepositoryError::new("unsupported repository compression")),
    }
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<(), RepositoryError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RepositoryError::new("filesystem path contains NUL"))?;
    let result = unsafe { libc::chown(path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}

fn set_canonical_times(path: &Path) -> Result<(), RepositoryError> {
    let stamp = libc::timespec {
        tv_sec: CANONICAL_TIMESTAMP as _,
        tv_nsec: 0,
    };
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RepositoryError::new("filesystem path contains NUL"))?;
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            [stamp, stamp].as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}

fn require_u32(value: u64, name: &str) -> Result<u32, RepositoryError> {
    value
        .try_into()
        .map_err(|_| RepositoryError::new(format!("fs-file {name} exceeds u32")))
}

fn require_length(actual: u64, expected: u64, what: &str) -> Result<(), RepositoryError> {
    if actual == expected {
        Ok(())
    } else {
        Err(RepositoryError::new(format!(
            "{what} must contain exactly {expected} entries"
        )))
    }
}

fn require_text<R: Read>(
    decoder: &mut StreamDecoder<R>,
    expected: &str,
) -> Result<(), RepositoryError> {
    if decoder.text()? == expected {
        Ok(())
    } else {
        Err(RepositoryError::new(format!(
            "unexpected or incorrectly ordered CBOR map key; expected '{expected}'"
        )))
    }
}

struct StreamDecoder<R> {
    input: R,
}

impl<R: Read> StreamDecoder<R> {
    fn new(input: R) -> Self {
        Self { input }
    }

    fn array(&mut self) -> Result<u64, RepositoryError> {
        self.head(4, "array")
    }

    fn map(&mut self) -> Result<u64, RepositoryError> {
        self.head(5, "map")
    }

    fn uint(&mut self) -> Result<u64, RepositoryError> {
        self.head(0, "unsigned integer")
    }

    fn bytes_length(&mut self) -> Result<u64, RepositoryError> {
        self.head(2, "byte string")
    }

    fn text(&mut self) -> Result<String, RepositoryError> {
        let length = self.head(3, "text string")?;
        if length > 128 {
            return Err(RepositoryError::new("repository metadata text is too long"));
        }
        let mut bytes = vec![0; length as usize];
        self.input.read_exact(&mut bytes)?;
        String::from_utf8(bytes)
            .map_err(|_| RepositoryError::new("CBOR text string is not valid UTF-8"))
    }

    fn boolean(&mut self) -> Result<bool, RepositoryError> {
        match self.byte()? {
            0xf4 => Ok(false),
            0xf5 => Ok(true),
            _ => Err(RepositoryError::new("expected a CBOR boolean")),
        }
    }

    fn position(&mut self) -> Result<u64, RepositoryError>
    where
        R: Seek,
    {
        Ok(self.input.stream_position()?)
    }

    fn head(&mut self, expected_major: u8, expected: &str) -> Result<u64, RepositoryError> {
        let initial = self.byte()?;
        if initial >> 5 != expected_major {
            return Err(RepositoryError::new(format!("expected a CBOR {expected}")));
        }
        let additional = initial & 0x1f;
        let value = match additional {
            0..=23 => additional as u64,
            24 => {
                let value = self.byte()? as u64;
                require_shortest(value >= 24)?;
                value
            }
            25 => {
                let value = u16::from_be_bytes(self.read_array()?) as u64;
                require_shortest(value > 0xff)?;
                value
            }
            26 => {
                let value = u32::from_be_bytes(self.read_array()?) as u64;
                require_shortest(value > 0xffff)?;
                value
            }
            27 => {
                let value = u64::from_be_bytes(self.read_array()?);
                require_shortest(value > 0xffff_ffff)?;
                value
            }
            31 => return Err(RepositoryError::new("indefinite-length CBOR is forbidden")),
            _ => return Err(RepositoryError::new("reserved CBOR additional information")),
        };
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, RepositoryError> {
        let mut byte = [0];
        self.input.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], RepositoryError> {
        let mut bytes = [0; N];
        self.input.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

fn require_shortest(valid: bool) -> Result<(), RepositoryError> {
    if valid {
        Ok(())
    } else {
        Err(RepositoryError::new(
            "CBOR integer or length is not shortest-encoded",
        ))
    }
}

fn write_text(output: &mut impl Write, text: &str) -> Result<(), RepositoryError> {
    write_head(output, 3, text.len() as u64)?;
    output.write_all(text.as_bytes())?;
    Ok(())
}

fn write_head(output: &mut impl Write, major: u8, value: u64) -> Result<(), RepositoryError> {
    let prefix = major << 5;
    match value {
        0..=23 => output.write_all(&[prefix | value as u8])?,
        24..=0xff => output.write_all(&[prefix | 24, value as u8])?,
        0x100..=0xffff => {
            output.write_all(&[prefix | 25])?;
            output.write_all(&(value as u16).to_be_bytes())?;
        }
        0x1_0000..=0xffff_ffff => {
            output.write_all(&[prefix | 26])?;
            output.write_all(&(value as u32).to_be_bytes())?;
        }
        _ => {
            output.write_all(&[prefix | 27])?;
            output.write_all(&value.to_be_bytes())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_objects_roundtrip_both_compressions() {
        for compression in [Compression::Identity, Compression::Zstd] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let encoded = temp.path().join("encoded");
            let decoded = temp.path().join("decoded");
            fs::write(&source, b"content repeated content repeated").unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o711)).unwrap();
            let hash = encode_object(&source, compression, &encoded).unwrap();
            assert_eq!(
                decode_object(&encoded, hash, &decoded).unwrap(),
                ObjectKind::File { executable: true }
            );
            assert_eq!(fs::read(decoded).unwrap(), fs::read(source).unwrap());
        }
    }

    #[test]
    fn directory_object_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        fs::write(source.join("file"), b"payload").unwrap();
        let encoded = temp.path().join("encoded");
        let decoded = temp.path().join("decoded");
        let hash = encode_object(&source, Compression::Zstd, &encoded).unwrap();
        assert_eq!(
            decode_object(&encoded, hash, &decoded).unwrap(),
            ObjectKind::Directory
        );
        assert_eq!(hash_path(source).unwrap(), hash_path(decoded).unwrap());
    }

    #[test]
    fn fs_file_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let encoded = temp.path().join("encoded");
        let decoded = temp.path().join("decoded");
        fs::write(&source, b"fs file").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let hash = encode_fs_file(&source, Compression::Zstd, &encoded).unwrap();
        let metadata = decode_fs_file(&encoded, hash, &decoded).unwrap();
        assert_eq!(metadata.mode, 0o640);
        assert_eq!(fs::read(decoded).unwrap(), b"fs file");
    }

    #[test]
    fn mismatched_identity_does_not_publish() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let encoded = temp.path().join("encoded");
        let decoded = temp.path().join("decoded");
        fs::write(&source, b"data").unwrap();
        encode_object(&source, Compression::Identity, &encoded).unwrap();
        assert!(decode_object(&encoded, ObjectHash::from_bytes([9; 32]), &decoded).is_err());
        assert!(!decoded.exists());
    }

    #[test]
    fn noncanonical_envelope_and_trailing_bytes_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let encoded = temp.path().join("encoded");
        fs::write(&source, b"data").unwrap();
        let hash = encode_object(&source, Compression::Identity, &encoded).unwrap();

        let canonical = fs::read(&encoded).unwrap();
        let mut noncanonical = vec![0x98, 0x02];
        noncanonical.extend_from_slice(&canonical[1..]);
        fs::write(&encoded, noncanonical).unwrap();
        assert!(decode_object(&encoded, hash, &temp.path().join("noncanonical")).is_err());

        let mut trailing = canonical;
        trailing.push(0);
        fs::write(&encoded, trailing).unwrap();
        assert!(decode_object(&encoded, hash, &temp.path().join("trailing")).is_err());
    }
}
