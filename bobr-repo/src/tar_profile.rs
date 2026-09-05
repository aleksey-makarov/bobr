//! Restricted streaming tar profile for repository directory objects.

use crate::{
    MAX_TAR_COMPONENT_BYTES, MAX_TAR_DEPTH, MAX_TAR_ENTRIES, MAX_TAR_PATH_BYTES,
    MAX_TAR_STRUCTURAL_BYTES, MAX_TAR_SYMLINK_TARGET_BYTES, RepositoryError,
};
use bobr_core::CANONICAL_TIMESTAMP;
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

const BLOCK: usize = 512;
const GNU_LONG_PATH: &[u8] = b"././@LongLink";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Directory,
    File,
    Symlink,
}

/// Writes a directory using the repository version 1 tar profile.
pub fn write_directory_tar(source: &Path, output: &mut impl Write) -> Result<u64, RepositoryError> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_dir() {
        return Err(RepositoryError::new(
            "directory tar source is not a directory",
        ));
    }
    let mut written = 0;
    let mut entries = 0;
    let mut structural_bytes = 0;
    write_children(
        source,
        &[],
        output,
        &mut written,
        &mut entries,
        &mut structural_bytes,
    )?;
    output.write_all(&[0; BLOCK * 2])?;
    written += (BLOCK * 2) as u64;
    Ok(written)
}

/// Parses and materializes one repository directory tar into an empty root.
pub fn extract_directory_tar(
    input: &mut impl Read,
    destination: &Path,
) -> Result<u64, RepositoryError> {
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    let mut state = ExtractState {
        destination,
        nodes: BTreeMap::new(),
        pending_name: None,
        pending_link: None,
        entries: 0,
        structural_bytes: 0,
        decoded: 0,
    };
    loop {
        let header = read_block(input, &mut state.decoded)?;
        if header == [0; BLOCK] {
            let second = read_block(input, &mut state.decoded)?;
            if second != [0; BLOCK] {
                return Err(RepositoryError::new(
                    "tar archive has only one terminating zero block",
                ));
            }
            let mut trailing = [0u8; 1];
            if input.read(&mut trailing)? != 0 {
                return Err(RepositoryError::new(
                    "tar archive contains bytes after its terminator",
                ));
            }
            break;
        }
        let parsed = parse_header(&header)?;
        match parsed.kind {
            b'L' | b'K' => state.read_long(input, &parsed)?,
            b'\0' | b'0' | b'2' | b'5' => state.extract_logical(input, parsed)?,
            kind => {
                return Err(RepositoryError::new(format!(
                    "unsupported repository tar type flag 0x{kind:02x}"
                )));
            }
        }
    }
    if state.pending_name.is_some() || state.pending_link.is_some() {
        return Err(RepositoryError::new(
            "GNU long-name extension has no following logical entry",
        ));
    }
    normalize_tree(destination)?;
    Ok(state.decoded)
}

struct ExtractState<'a> {
    destination: &'a Path,
    nodes: BTreeMap<Vec<u8>, NodeKind>,
    pending_name: Option<Vec<u8>>,
    pending_link: Option<Vec<u8>>,
    entries: u64,
    structural_bytes: u64,
    decoded: u64,
}

struct ParsedHeader {
    path: Vec<u8>,
    link: Vec<u8>,
    mode: u64,
    size: u64,
    kind: u8,
}

impl ExtractState<'_> {
    fn read_long(
        &mut self,
        input: &mut impl Read,
        header: &ParsedHeader,
    ) -> Result<(), RepositoryError> {
        if header.path != GNU_LONG_PATH {
            return Err(RepositoryError::new(
                "GNU long-name extension has an invalid header path",
            ));
        }
        if header.kind == b'L' {
            if self.pending_name.is_some() || self.pending_link.is_some() {
                return Err(RepositoryError::new(
                    "duplicate or incorrectly ordered GNU LongName entry",
                ));
            }
        } else if self.pending_link.is_some() {
            return Err(RepositoryError::new("duplicate GNU LongLink entry"));
        }
        let limit = if header.kind == b'L' {
            MAX_TAR_PATH_BYTES
        } else {
            MAX_TAR_SYMLINK_TARGET_BYTES
        };
        let value = read_entry_bytes(input, header.size, limit + 1, &mut self.decoded)?;
        if value.len() < 2 || value.last() != Some(&0) || value[..value.len() - 1].contains(&0) {
            return Err(RepositoryError::new(
                "GNU long-name data must end in exactly one NUL",
            ));
        }
        let value = value[..value.len() - 1].to_vec();
        if header.kind == b'L' {
            if ustar_path_fields(&value).is_some() {
                return Err(RepositoryError::new(
                    "GNU LongName is forbidden when the path fits in ustar fields",
                ));
            }
            self.pending_name = Some(value);
        } else {
            if value.len() <= 100 {
                return Err(RepositoryError::new(
                    "GNU LongLink is forbidden when the target fits in ustar linkname",
                ));
            }
            self.pending_link = Some(value);
        }
        Ok(())
    }

    fn extract_logical(
        &mut self,
        input: &mut impl Read,
        header: ParsedHeader,
    ) -> Result<(), RepositoryError> {
        self.entries += 1;
        if self.entries > MAX_TAR_ENTRIES {
            return Err(RepositoryError::new(
                "repository tar contains too many entries",
            ));
        }
        let path = normalize_path(
            self.pending_name.take().unwrap_or(header.path),
            header.kind == b'5',
        )?;
        let link = self.pending_link.take().unwrap_or(header.link);
        let kind = match header.kind {
            b'\0' | b'0' => NodeKind::File,
            b'2' => NodeKind::Symlink,
            b'5' => NodeKind::Directory,
            _ => unreachable!(),
        };
        if kind != NodeKind::Symlink && !link.is_empty() {
            return Err(RepositoryError::new(
                "non-symlink tar entry has a link target",
            ));
        }
        if kind == NodeKind::Symlink {
            if link.is_empty() {
                return Err(RepositoryError::new("tar symlink target is empty"));
            }
            if link.len() > MAX_TAR_SYMLINK_TARGET_BYTES {
                return Err(RepositoryError::new("tar symlink target is too long"));
            }
        }
        self.structural_bytes = self
            .structural_bytes
            .checked_add(path.len() as u64)
            .and_then(|size| size.checked_add(link.len() as u64))
            .ok_or_else(|| RepositoryError::new("tar structural size overflow"))?;
        if self.structural_bytes > MAX_TAR_STRUCTURAL_BYTES {
            return Err(RepositoryError::new(
                "repository tar structural metadata exceeds format limit",
            ));
        }
        if kind != NodeKind::File && header.size != 0 {
            return Err(RepositoryError::new(
                "directory or symbolic-link tar entry has nonzero size",
            ));
        }
        self.insert_path(&path, kind)?;
        let destination = join_raw(self.destination, &path);
        match kind {
            NodeKind::Directory => {
                fs::create_dir_all(&destination)?;
                skip_entry_bytes(input, header.size, &mut self.decoded)?;
            }
            NodeKind::File => {
                let mut file = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&destination)?;
                copy_entry_bytes(input, &mut file, header.size, &mut self.decoded)?;
                let mode = if header.mode & 0o111 != 0 {
                    0o755
                } else {
                    0o644
                };
                fs::set_permissions(&destination, fs::Permissions::from_mode(mode))?;
            }
            NodeKind::Symlink => {
                symlink(OsString::from_vec(link), &destination)?;
                skip_entry_bytes(input, header.size, &mut self.decoded)?;
            }
        }
        Ok(())
    }

    fn insert_path(&mut self, path: &[u8], kind: NodeKind) -> Result<(), RepositoryError> {
        let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
        let mut parent = Vec::new();
        for component in &components[..components.len() - 1] {
            if !parent.is_empty() {
                parent.push(b'/');
            }
            parent.extend_from_slice(component);
            match self.nodes.get(&parent) {
                Some(NodeKind::Directory) => {}
                Some(_) => {
                    return Err(RepositoryError::new("tar path has a non-directory parent"));
                }
                None => {
                    self.nodes.insert(parent.clone(), NodeKind::Directory);
                    fs::create_dir_all(join_raw(self.destination, &parent))?;
                }
            }
        }
        match self.nodes.get(path) {
            Some(NodeKind::Directory) if kind == NodeKind::Directory => Ok(()),
            Some(_) => Err(RepositoryError::new("duplicate or conflicting tar path")),
            None => {
                self.nodes.insert(path.to_vec(), kind);
                Ok(())
            }
        }
    }
}

fn parse_header(header: &[u8; BLOCK]) -> Result<ParsedHeader, RepositoryError> {
    validate_checksum(header)?;
    if &header[257..263] != b"ustar\0" || &header[263..265] != b"00" {
        return Err(RepositoryError::new("tar entry is not POSIX ustar"));
    }
    let name = nul_field(&header[0..100], "name")?;
    let mode = parse_octal(&header[100..108], "mode")?;
    parse_octal(&header[108..116], "uid")?;
    parse_octal(&header[116..124], "gid")?;
    let size = parse_size(&header[124..136])?;
    parse_octal(&header[136..148], "mtime")?;
    let kind = header[156];
    let link = nul_field(&header[157..257], "linkname")?;
    let prefix = nul_field(&header[345..500], "prefix")?;
    parse_octal_empty(&header[329..337], "device major")?;
    parse_octal_empty(&header[337..345], "device minor")?;
    let path = if prefix.is_empty() {
        name
    } else {
        let mut path = prefix;
        path.push(b'/');
        path.extend_from_slice(&name);
        path
    };
    Ok(ParsedHeader {
        path,
        link,
        mode,
        size,
        kind,
    })
}

fn validate_checksum(header: &[u8; BLOCK]) -> Result<(), RepositoryError> {
    let expected = parse_octal(&header[148..156], "checksum")?;
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                b' ' as u64
            } else {
                *byte as u64
            }
        })
        .sum::<u64>();
    if actual != expected {
        return Err(RepositoryError::new("invalid tar header checksum"));
    }
    Ok(())
}

fn parse_octal(field: &[u8], name: &str) -> Result<u64, RepositoryError> {
    let mut value = 0u64;
    let mut saw_digit = false;
    let mut ended = false;
    for byte in field {
        match *byte {
            b' ' if !saw_digit => {}
            b'0'..=b'7' if !ended => {
                saw_digit = true;
                value = value
                    .checked_mul(8)
                    .and_then(|value| value.checked_add((byte - b'0') as u64))
                    .ok_or_else(|| RepositoryError::new(format!("tar {name} overflows u64")))?;
            }
            0 | b' ' if saw_digit => ended = true,
            _ => return Err(RepositoryError::new(format!("invalid tar {name} field"))),
        }
    }
    if saw_digit {
        Ok(value)
    } else {
        Err(RepositoryError::new(format!("empty tar {name} field")))
    }
}

fn parse_octal_empty(field: &[u8], name: &str) -> Result<(), RepositoryError> {
    if field.iter().all(|byte| *byte == 0 || *byte == b' ') {
        Ok(())
    } else {
        parse_octal(field, name).map(|_| ())
    }
}

fn parse_size(field: &[u8]) -> Result<u64, RepositoryError> {
    if field[0] & 0x80 == 0 {
        return parse_octal(field, "size");
    }
    if field[0] & 0x40 != 0 {
        return Err(RepositoryError::new("negative base-256 tar size"));
    }
    let mut value = (field[0] & 0x3f) as u64;
    for byte in &field[1..] {
        value = value
            .checked_mul(256)
            .and_then(|value| value.checked_add(*byte as u64))
            .ok_or_else(|| RepositoryError::new("base-256 tar size overflows u64"))?;
    }
    let octal_max = (1u64 << (3 * (field.len() - 1))) - 1;
    if value <= octal_max {
        return Err(RepositoryError::new(
            "base-256 tar size is forbidden when octal encoding fits",
        ));
    }
    Ok(value)
}

fn nul_field(field: &[u8], name: &str) -> Result<Vec<u8>, RepositoryError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if field[end..].iter().any(|byte| *byte != 0) {
        return Err(RepositoryError::new(format!(
            "tar {name} contains bytes after NUL"
        )));
    }
    Ok(field[..end].to_vec())
}

fn normalize_path(mut path: Vec<u8>, directory: bool) -> Result<Vec<u8>, RepositoryError> {
    if path.first() == Some(&b'/') {
        return Err(RepositoryError::new("absolute tar path is forbidden"));
    }
    if directory {
        while path.last() == Some(&b'/') {
            path.pop();
        }
    }
    let mut normalized = Vec::new();
    let mut depth = 0;
    for component in path.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            return Err(RepositoryError::new("parent component in tar path"));
        }
        if component.len() > MAX_TAR_COMPONENT_BYTES {
            return Err(RepositoryError::new("tar path component is too long"));
        }
        if !normalized.is_empty() {
            normalized.push(b'/');
        }
        normalized.extend_from_slice(component);
        depth += 1;
    }
    if normalized.is_empty() {
        return Err(RepositoryError::new("tar path normalizes to empty"));
    }
    if normalized.len() > MAX_TAR_PATH_BYTES || depth > MAX_TAR_DEPTH {
        return Err(RepositoryError::new(
            "normalized tar path exceeds format limits",
        ));
    }
    Ok(normalized)
}

fn read_block(input: &mut impl Read, decoded: &mut u64) -> Result<[u8; BLOCK], RepositoryError> {
    let mut block = [0u8; BLOCK];
    input.read_exact(&mut block)?;
    *decoded = decoded
        .checked_add(BLOCK as u64)
        .ok_or_else(|| RepositoryError::new("decoded tar size overflow"))?;
    Ok(block)
}

fn read_entry_bytes(
    input: &mut impl Read,
    size: u64,
    limit: usize,
    decoded: &mut u64,
) -> Result<Vec<u8>, RepositoryError> {
    if size > limit as u64 {
        return Err(RepositoryError::new(
            "tar extension value exceeds format limit",
        ));
    }
    let mut bytes = vec![0; size as usize];
    input.read_exact(&mut bytes)?;
    read_padding(input, size, decoded)?;
    *decoded = decoded
        .checked_add(size)
        .ok_or_else(|| RepositoryError::new("decoded tar size overflow"))?;
    Ok(bytes)
}

fn copy_entry_bytes(
    input: &mut impl Read,
    output: &mut impl Write,
    size: u64,
    decoded: &mut u64,
) -> Result<(), RepositoryError> {
    let copied = io::copy(&mut input.take(size), output)?;
    if copied != size {
        return Err(RepositoryError::new("truncated tar entry data"));
    }
    *decoded = decoded
        .checked_add(size)
        .ok_or_else(|| RepositoryError::new("decoded tar size overflow"))?;
    read_padding(input, size, decoded)
}

fn skip_entry_bytes(
    input: &mut impl Read,
    size: u64,
    decoded: &mut u64,
) -> Result<(), RepositoryError> {
    let copied = io::copy(&mut input.take(size), &mut io::sink())?;
    if copied != size {
        return Err(RepositoryError::new("truncated tar entry data"));
    }
    *decoded = decoded
        .checked_add(size)
        .ok_or_else(|| RepositoryError::new("decoded tar size overflow"))?;
    read_padding(input, size, decoded)
}

fn read_padding(
    input: &mut impl Read,
    size: u64,
    decoded: &mut u64,
) -> Result<(), RepositoryError> {
    let padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
    let mut remaining = padding;
    let mut buffer = [0u8; BLOCK];
    while remaining > 0 {
        let take = remaining.min(BLOCK as u64) as usize;
        input.read_exact(&mut buffer[..take])?;
        if buffer[..take].iter().any(|byte| *byte != 0) {
            return Err(RepositoryError::new("nonzero tar entry padding"));
        }
        remaining -= take as u64;
    }
    *decoded = decoded
        .checked_add(padding)
        .ok_or_else(|| RepositoryError::new("decoded tar size overflow"))?;
    Ok(())
}

fn write_children(
    root: &Path,
    relative: &[u8],
    output: &mut impl Write,
    written: &mut u64,
    entry_count: &mut u64,
    structural_bytes: &mut u64,
) -> Result<(), RepositoryError> {
    let directory = join_raw(root, relative);
    let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for entry in entries {
        let name = entry.file_name();
        let mut path = relative.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name.as_bytes());
        normalize_path(path.clone(), false)?;
        *entry_count += 1;
        if *entry_count > MAX_TAR_ENTRIES {
            return Err(RepositoryError::new(
                "repository tar contains too many entries",
            ));
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        let file_type = metadata.file_type();
        let link_target = if file_type.is_symlink() {
            Some(fs::read_link(entry.path())?)
        } else {
            None
        };
        if link_target.as_ref().is_some_and(|target| {
            target.as_os_str().as_bytes().len() > MAX_TAR_SYMLINK_TARGET_BYTES
        }) {
            return Err(RepositoryError::new("tar symlink target is too long"));
        }
        *structural_bytes = structural_bytes
            .checked_add(path.len() as u64)
            .and_then(|size| {
                size.checked_add(
                    link_target
                        .as_ref()
                        .map(|target| target.as_os_str().as_bytes().len() as u64)
                        .unwrap_or(0),
                )
            })
            .ok_or_else(|| RepositoryError::new("tar structural size overflow"))?;
        if *structural_bytes > MAX_TAR_STRUCTURAL_BYTES {
            return Err(RepositoryError::new(
                "repository tar structural metadata exceeds format limit",
            ));
        }
        if file_type.is_dir() {
            write_header_and_data(output, &path, b'5', 0o755, &[], &[], written)?;
            write_children(root, &path, output, written, entry_count, structural_bytes)?;
        } else if file_type.is_file() {
            let mode = if metadata.mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            let size = metadata.size();
            write_header(output, &path, b'0', mode, size, &[], written)?;
            let copied = io::copy(&mut fs::File::open(entry.path())?, output)?;
            if copied != size {
                return Err(RepositoryError::new("file changed while writing tar"));
            }
            *written += copied;
            write_zero_padding(output, size, written)?;
        } else if file_type.is_symlink() {
            let target = link_target.expect("symlink target was read");
            write_header_and_data(
                output,
                &path,
                b'2',
                0o777,
                target.as_os_str().as_bytes(),
                &[],
                written,
            )?;
        } else if file_type.is_socket()
            || file_type.is_fifo()
            || file_type.is_block_device()
            || file_type.is_char_device()
        {
            return Err(RepositoryError::new(
                "unsupported filesystem kind in directory object",
            ));
        }
    }
    Ok(())
}

fn write_header_and_data(
    output: &mut impl Write,
    path: &[u8],
    kind: u8,
    mode: u32,
    link: &[u8],
    data: &[u8],
    written: &mut u64,
) -> Result<(), RepositoryError> {
    write_header(output, path, kind, mode, data.len() as u64, link, written)?;
    if !data.is_empty() {
        output.write_all(data)?;
        *written += data.len() as u64;
        write_zero_padding(output, data.len() as u64, written)?;
    }
    Ok(())
}

fn write_header(
    output: &mut impl Write,
    path: &[u8],
    kind: u8,
    mode: u32,
    size: u64,
    link: &[u8],
    written: &mut u64,
) -> Result<(), RepositoryError> {
    if ustar_path_fields(path).is_none() {
        let mut value = path.to_vec();
        value.push(0);
        write_raw_header(output, GNU_LONG_PATH, b'L', 0o644, value.len() as u64, &[])?;
        output.write_all(&value)?;
        *written += BLOCK as u64 + value.len() as u64;
        write_zero_padding(output, value.len() as u64, written)?;
    }
    if link.len() > 100 {
        let mut value = link.to_vec();
        value.push(0);
        write_raw_header(output, GNU_LONG_PATH, b'K', 0o644, value.len() as u64, &[])?;
        output.write_all(&value)?;
        *written += BLOCK as u64 + value.len() as u64;
        write_zero_padding(output, value.len() as u64, written)?;
    }
    write_raw_header(output, path, kind, mode, size, link)?;
    *written += BLOCK as u64;
    Ok(())
}

fn write_raw_header(
    output: &mut impl Write,
    path: &[u8],
    kind: u8,
    mode: u32,
    size: u64,
    link: &[u8],
) -> Result<(), RepositoryError> {
    let (name, prefix) = ustar_path_fields(path).unwrap_or((b"placeholder".as_slice(), &[]));
    let mut header = [0u8; BLOCK];
    header[..name.len()].copy_from_slice(name);
    write_octal(&mut header[100..108], mode as u64)?;
    write_octal(&mut header[108..116], 0)?;
    write_octal(&mut header[116..124], 0)?;
    write_size(&mut header[124..136], size)?;
    write_octal(&mut header[136..148], CANONICAL_TIMESTAMP as u64)?;
    header[148..156].fill(b' ');
    header[156] = kind;
    if link.len() <= 100 {
        header[157..157 + link.len()].copy_from_slice(link);
    } else {
        header[157..168].copy_from_slice(b"placeholder");
    }
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[345..345 + prefix.len()].copy_from_slice(prefix);
    let checksum: u64 = header.iter().map(|byte| *byte as u64).sum();
    let checksum_text = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum_text.as_bytes());
    output.write_all(&header)?;
    Ok(())
}

fn write_octal(field: &mut [u8], value: u64) -> Result<(), RepositoryError> {
    let digits = field.len() - 1;
    let value = format!("{value:0digits$o}");
    if value.len() > digits {
        return Err(RepositoryError::new(
            "value does not fit in ustar octal field",
        ));
    }
    field[..digits].copy_from_slice(value.as_bytes());
    field[digits] = 0;
    Ok(())
}

fn write_size(field: &mut [u8], value: u64) -> Result<(), RepositoryError> {
    let octal_max = (1u64 << (3 * (field.len() - 1))) - 1;
    if value <= octal_max {
        return write_octal(field, value);
    }
    let bytes = value.to_be_bytes();
    if field.len() < bytes.len() {
        return Err(RepositoryError::new("file is too large for tar size field"));
    }
    field.fill(0);
    let offset = field.len() - bytes.len();
    field[offset..].copy_from_slice(&bytes);
    field[0] |= 0x80;
    Ok(())
}

fn write_zero_padding(
    output: &mut impl Write,
    size: u64,
    written: &mut u64,
) -> Result<(), RepositoryError> {
    let padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
    output.write_all(&[0; BLOCK][..padding as usize])?;
    *written += padding;
    Ok(())
}

fn ustar_path_fields(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() <= 100 {
        return Some((path, &[]));
    }
    if path.len() > 256 {
        return None;
    }
    path.iter()
        .enumerate()
        .rev()
        .find(|(index, byte)| **byte == b'/' && *index <= 155 && path.len() - index - 1 <= 100)
        .map(|(index, _)| (&path[index + 1..], &path[..index]))
}

fn join_raw(root: &Path, relative: &[u8]) -> PathBuf {
    let mut result = root.to_path_buf();
    for component in relative.split(|byte| *byte == b'/') {
        if !component.is_empty() {
            result.push(OsStr::from_bytes(component));
        }
    }
    result
}

fn normalize_tree(root: &Path) -> Result<(), RepositoryError> {
    let mut paths = Vec::new();
    collect_paths(root, &mut paths)?;
    for path in &paths {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
    }
    paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in paths {
        set_canonical_times(&path)?;
    }
    Ok(())
}

fn collect_paths(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), RepositoryError> {
    paths.push(root.to_path_buf());
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if fs::symlink_metadata(&path)?.file_type().is_dir() {
            collect_paths(&path, paths)?;
        } else {
            paths.push(path);
        }
    }
    Ok(())
}

fn set_canonical_times(path: &Path) -> Result<(), RepositoryError> {
    let stamp = libc::timespec {
        tv_sec: CANONICAL_TIMESTAMP as _,
        tv_nsec: 0,
    };
    let times = [stamp, stamp];
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RepositoryError::new("filesystem path contains NUL"))?;
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_tar_roundtrips_raw_names_and_symlinks() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("empty")).unwrap();
        fs::write(source.path().join("plain"), b"hello").unwrap();
        fs::write(source.path().join("tool"), b"run").unwrap();
        fs::set_permissions(
            source.path().join("tool"),
            fs::Permissions::from_mode(0o711),
        )
        .unwrap();
        symlink("plain", source.path().join("link")).unwrap();
        fs::write(source.path().join(OsStr::from_bytes(b"raw-\xff")), b"bytes").unwrap();

        let mut archive = Vec::new();
        write_directory_tar(source.path(), &mut archive).unwrap();
        let destination_parent = tempfile::tempdir().unwrap();
        let destination = destination_parent.path().join("tree");
        extract_directory_tar(&mut archive.as_slice(), &destination).unwrap();
        assert_eq!(
            fsobj_hash::hash_path(source.path()).unwrap(),
            fsobj_hash::hash_path(&destination).unwrap()
        );
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("plain")
        );
        assert_eq!(
            fs::metadata(destination.join("tool")).unwrap().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn extraction_rejects_trailing_and_bad_padding() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("file"), b"x").unwrap();
        let mut archive = Vec::new();
        write_directory_tar(source.path(), &mut archive).unwrap();
        archive.push(0);
        let parent = tempfile::tempdir().unwrap();
        assert!(
            extract_directory_tar(&mut archive.as_slice(), &parent.path().join("tree")).is_err()
        );
    }

    #[test]
    fn long_name_and_long_link_roundtrip() {
        let source = tempfile::tempdir().unwrap();
        let first = "a".repeat(180);
        let second = "b".repeat(90);
        fs::create_dir(source.path().join(&first)).unwrap();
        fs::write(source.path().join(&first).join(&second), b"long").unwrap();
        let target = "target".repeat(30);
        symlink(&target, source.path().join("long-link")).unwrap();

        let mut archive = Vec::new();
        write_directory_tar(source.path(), &mut archive).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("tree");
        extract_directory_tar(&mut archive.as_slice(), &destination).unwrap();
        assert_eq!(
            fsobj_hash::hash_path(source.path()).unwrap(),
            fsobj_hash::hash_path(destination).unwrap()
        );
    }

    #[test]
    fn traversal_and_symlink_parent_are_rejected() {
        let mut traversal = Vec::new();
        let mut written = 0;
        write_header(
            &mut traversal,
            b"../outside",
            b'0',
            0o644,
            0,
            &[],
            &mut written,
        )
        .unwrap();
        traversal.extend_from_slice(&[0; BLOCK * 2]);
        let parent = tempfile::tempdir().unwrap();
        assert!(
            extract_directory_tar(&mut traversal.as_slice(), &parent.path().join("tree")).is_err()
        );
        assert!(!parent.path().join("outside").exists());

        let mut conflict = Vec::new();
        written = 0;
        write_header(
            &mut conflict,
            b"link",
            b'2',
            0o777,
            0,
            b"target",
            &mut written,
        )
        .unwrap();
        write_header(
            &mut conflict,
            b"link/child",
            b'0',
            0o644,
            0,
            &[],
            &mut written,
        )
        .unwrap();
        conflict.extend_from_slice(&[0; BLOCK * 2]);
        assert!(
            extract_directory_tar(&mut conflict.as_slice(), &parent.path().join("tree2")).is_err()
        );
    }

    #[test]
    fn one_terminator_block_is_rejected() {
        let mut archive = vec![0; BLOCK];
        archive.extend_from_slice(&[1; BLOCK]);
        let parent = tempfile::tempdir().unwrap();
        assert!(
            extract_directory_tar(&mut archive.as_slice(), &parent.path().join("tree")).is_err()
        );
    }
}
