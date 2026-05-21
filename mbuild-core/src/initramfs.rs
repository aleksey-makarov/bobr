use crate::FsTreeEntry;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

const CPIO_NEWC_MAGIC: &[u8; 6] = b"070701";
const CPIO_TRAILER: &str = "TRAILER!!!";
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const SYMLINK_MODE: u32 = 0o777;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitramfsEntrySource {
    Directory,
    File { path: PathBuf },
    Symlink,
}

#[derive(Debug)]
pub enum InitramfsError {
    Invalid(String),
    Io(String),
}

impl fmt::Display for InitramfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for InitramfsError {}

pub fn write_newc_initramfs<W: Write>(
    writer: W,
    entries: &[FsTreeEntry],
    sources: &[InitramfsEntrySource],
) -> Result<(), InitramfsError> {
    if entries.len() != sources.len() {
        return Err(InitramfsError::Invalid(format!(
            "initramfs source count {} does not match manifest entry count {}",
            sources.len(),
            entries.len()
        )));
    }

    let mut writer = io::BufWriter::new(writer);
    let mut ino = 1_u32;
    for (entry, source) in entries.iter().zip(sources) {
        write_entry(&mut writer, ino, entry, source)?;
        ino = ino.checked_add(1).ok_or_else(|| {
            InitramfsError::Invalid("initramfs entry inode counter overflowed".to_string())
        })?;
    }
    write_trailer(&mut writer, ino)?;
    writer
        .flush()
        .map_err(|error| InitramfsError::Io(format!("failed to flush initramfs: {error}")))?;
    Ok(())
}

fn write_entry<W: Write>(
    writer: &mut W,
    ino: u32,
    entry: &FsTreeEntry,
    source: &InitramfsEntrySource,
) -> Result<(), InitramfsError> {
    match (entry, source) {
        (
            FsTreeEntry::Directory {
                path,
                uid,
                gid,
                mode,
            },
            InitramfsEntrySource::Directory,
        ) => write_header_and_payload(
            writer,
            Header {
                ino,
                mode: S_IFDIR | *mode,
                uid: *uid,
                gid: *gid,
                nlink: 2,
                mtime: 0,
                filesize: 0,
                name: archive_path(path),
            },
            io::empty(),
        ),
        (
            FsTreeEntry::File {
                path,
                uid,
                gid,
                mode,
            },
            InitramfsEntrySource::File { path: source_path },
        ) => {
            let metadata = fs::metadata(source_path).map_err(|error| {
                InitramfsError::Io(format!(
                    "failed to stat initramfs source file '{}': {error}",
                    source_path.display()
                ))
            })?;
            let mut file = fs::File::open(source_path).map_err(|error| {
                InitramfsError::Io(format!(
                    "failed to open initramfs source file '{}': {error}",
                    source_path.display()
                ))
            })?;
            let filesize = u32::try_from(metadata.len()).map_err(|_| {
                InitramfsError::Invalid(format!(
                    "initramfs source file '{}' is too large for newc",
                    source_path.display()
                ))
            })?;
            write_header_and_payload(
                writer,
                Header {
                    ino,
                    mode: S_IFREG | *mode,
                    uid: *uid,
                    gid: *gid,
                    nlink: 1,
                    mtime: 0,
                    filesize,
                    name: archive_path(path),
                },
                &mut file,
            )
        }
        (
            FsTreeEntry::Symlink {
                path,
                uid,
                gid,
                target,
            },
            InitramfsEntrySource::Symlink,
        ) => {
            let filesize = u32::try_from(target.len()).map_err(|_| {
                InitramfsError::Invalid(format!(
                    "initramfs symlink target for '{path}' is too large for newc"
                ))
            })?;
            write_header_and_payload(
                writer,
                Header {
                    ino,
                    mode: S_IFLNK | SYMLINK_MODE,
                    uid: *uid,
                    gid: *gid,
                    nlink: 1,
                    mtime: 0,
                    filesize,
                    name: archive_path(path),
                },
                target.as_bytes(),
            )
        }
        _ => Err(InitramfsError::Invalid(format!(
            "initramfs source kind does not match manifest entry '{}'",
            entry.path()
        ))),
    }
}

fn write_trailer<W: Write>(writer: &mut W, ino: u32) -> Result<(), InitramfsError> {
    write_header_and_payload(
        writer,
        Header {
            ino,
            mode: 0,
            uid: 0,
            gid: 0,
            nlink: 1,
            mtime: 0,
            filesize: 0,
            name: CPIO_TRAILER.to_string(),
        },
        io::empty(),
    )
}

struct Header {
    ino: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u32,
    filesize: u32,
    name: String,
}

fn write_header_and_payload<W: Write, R: io::Read>(
    writer: &mut W,
    header: Header,
    mut payload: R,
) -> Result<(), InitramfsError> {
    let namesize = u32::try_from(header.name.len() + 1).map_err(|_| {
        InitramfsError::Invalid(format!(
            "initramfs path '{}' is too long for newc",
            header.name
        ))
    })?;

    writer
        .write_all(CPIO_NEWC_MAGIC)
        .and_then(|()| write_hex_u32(writer, header.ino))
        .and_then(|()| write_hex_u32(writer, header.mode))
        .and_then(|()| write_hex_u32(writer, header.uid))
        .and_then(|()| write_hex_u32(writer, header.gid))
        .and_then(|()| write_hex_u32(writer, header.nlink))
        .and_then(|()| write_hex_u32(writer, header.mtime))
        .and_then(|()| write_hex_u32(writer, header.filesize))
        .and_then(|()| write_hex_u32(writer, 0))
        .and_then(|()| write_hex_u32(writer, 0))
        .and_then(|()| write_hex_u32(writer, 0))
        .and_then(|()| write_hex_u32(writer, 0))
        .and_then(|()| write_hex_u32(writer, namesize))
        .and_then(|()| write_hex_u32(writer, 0))
        .map_err(|error| {
            InitramfsError::Io(format!(
                "failed to write initramfs header for '{}': {error}",
                header.name
            ))
        })?;

    writer
        .write_all(header.name.as_bytes())
        .and_then(|()| writer.write_all(&[0]))
        .map_err(|error| {
            InitramfsError::Io(format!(
                "failed to write initramfs path '{}': {error}",
                header.name
            ))
        })?;
    write_padding(writer, 110 + namesize as usize)?;

    let copied = io::copy(&mut payload, &mut *writer).map_err(|error| {
        InitramfsError::Io(format!(
            "failed to write initramfs payload for '{}': {error}",
            header.name
        ))
    })?;
    if copied != u64::from(header.filesize) {
        return Err(InitramfsError::Io(format!(
            "initramfs payload for '{}' changed while reading: expected {} bytes, copied {}",
            header.name, header.filesize, copied
        )));
    }
    write_padding(writer, header.filesize as usize)
}

fn write_hex_u32<W: Write>(writer: &mut W, value: u32) -> io::Result<()> {
    write!(writer, "{value:08x}")
}

fn write_padding<W: Write>(writer: &mut W, size: usize) -> Result<(), InitramfsError> {
    const ZEROES: [u8; 3] = [0; 3];
    let padding = (4 - size % 4) % 4;
    writer
        .write_all(&ZEROES[..padding])
        .map_err(|error| InitramfsError::Io(format!("failed to write initramfs padding: {error}")))
}

fn archive_path(path: &str) -> String {
    if path.is_empty() {
        ".".to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FsTreeEntry;
    use std::fs;
    use tempfile::tempdir;

    #[derive(Debug, PartialEq, Eq)]
    struct ParsedEntry {
        name: String,
        mode: u32,
        uid: u32,
        gid: u32,
        mtime: u32,
        data: Vec<u8>,
    }

    #[test]
    fn writes_deterministic_newc_archive() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("init");
        fs::write(&file_path, b"#!/bin/sh\n").unwrap();
        let entries = vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("bin", 1, 2, 0o755),
            FsTreeEntry::file("init", 3, 4, 0o755),
            FsTreeEntry::symlink("bin/sh", 5, 6, "../init"),
        ];
        let sources = vec![
            InitramfsEntrySource::Directory,
            InitramfsEntrySource::Directory,
            InitramfsEntrySource::File {
                path: file_path.clone(),
            },
            InitramfsEntrySource::Symlink,
        ];

        let mut first = Vec::new();
        write_newc_initramfs(&mut first, &entries, &sources).unwrap();
        let mut second = Vec::new();
        write_newc_initramfs(&mut second, &entries, &sources).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len() % 4, 0);
        let parsed = parse_newc(&first);
        assert_eq!(
            parsed
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            [".", "bin", "init", "bin/sh", "TRAILER!!!"]
        );
        assert_eq!(parsed[0].mode, S_IFDIR | 0o755);
        assert_eq!(parsed[1].uid, 1);
        assert_eq!(parsed[1].gid, 2);
        assert_eq!(parsed[2].mode, S_IFREG | 0o755);
        assert_eq!(parsed[2].uid, 3);
        assert_eq!(parsed[2].gid, 4);
        assert_eq!(parsed[2].mtime, 0);
        assert_eq!(parsed[2].data, b"#!/bin/sh\n");
        assert_eq!(parsed[3].mode, S_IFLNK | 0o777);
        assert_eq!(parsed[3].uid, 5);
        assert_eq!(parsed[3].gid, 6);
        assert_eq!(parsed[3].data, b"../init");
    }

    #[test]
    fn rejects_mismatched_sources() {
        let mut out = Vec::new();
        let error = write_newc_initramfs(&mut out, &[FsTreeEntry::directory("", 0, 0, 0o755)], &[])
            .unwrap_err();

        assert!(error.to_string().contains("source count"));
    }

    fn parse_newc(bytes: &[u8]) -> Vec<ParsedEntry> {
        let mut entries = Vec::new();
        let mut offset = 0_usize;
        loop {
            assert_eq!(&bytes[offset..offset + 6], CPIO_NEWC_MAGIC);
            let mode = parse_hex(&bytes[offset + 14..offset + 22]);
            let uid = parse_hex(&bytes[offset + 22..offset + 30]);
            let gid = parse_hex(&bytes[offset + 30..offset + 38]);
            let mtime = parse_hex(&bytes[offset + 46..offset + 54]);
            let filesize = parse_hex(&bytes[offset + 54..offset + 62]) as usize;
            let namesize = parse_hex(&bytes[offset + 94..offset + 102]) as usize;
            offset += 110;
            let name_bytes = &bytes[offset..offset + namesize - 1];
            assert_eq!(bytes[offset + namesize - 1], 0);
            let name = std::str::from_utf8(name_bytes).unwrap().to_string();
            offset += namesize;
            offset = align4(offset);
            let data = bytes[offset..offset + filesize].to_vec();
            offset += filesize;
            offset = align4(offset);
            entries.push(ParsedEntry {
                name: name.clone(),
                mode,
                uid,
                gid,
                mtime,
                data,
            });
            if name == CPIO_TRAILER {
                assert_eq!(offset, bytes.len());
                break;
            }
        }
        entries
    }

    fn parse_hex(bytes: &[u8]) -> u32 {
        u32::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap()
    }

    fn align4(value: usize) -> usize {
        (value + 3) & !3
    }
}
