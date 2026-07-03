use crate::BuilderError;
use crate::{BuildContext, BuilderInputs, InputSpec, StagedBuildResult, TypedBuilder};
use bobr_core::BuildLogLevel;
use bobr_runtime::runtime::{Runtime, RuntimeError, RuntimeFunction};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const OUTPUT_FILE_NAME: &str = "initramfs.img";
const CPIO_NEWC_MAGIC: &[u8; 6] = b"070701";
const CPIO_TRAILER: &str = "TRAILER!!!";
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const SYMLINK_MODE: u32 = 0o777;

/// Builds an initramfs cpio archive from an fs-tree (the `tree` input).
#[derive(Debug)]
pub struct InitramfsBuilder;

/// Configuration for [`InitramfsBuilder`] (no options).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitramfsConfig {}

static INITRAMFS_SPEC: InputSpec = InputSpec {
    required_inputs: &["_tree"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for InitramfsBuilder {
    type Config = InitramfsConfig;

    fn tag(&self) -> &'static str {
        "Initramfs"
    }

    fn impl_version(&self) -> &'static str {
        "1"
    }

    fn spec(&self) -> &'static InputSpec {
        &INITRAMFS_SPEC
    }

    fn build_typed(
        &self,
        _config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        let source_root = inputs.required("_tree")?.path.clone();
        let output_path = cx.temp_dir.join(OUTPUT_FILE_NAME);

        cx.log_event(
            BuildLogLevel::Info,
            "initramfs",
            format!(
                "writing deterministic initramfs '{}' from materialized fs-tree root '{}'",
                output_path.display(),
                source_root.display()
            ),
        );

        cx.runtime()
            .run(
                &InitramfsFunction,
                InitramfsInput {
                    source_root,
                    output_path: output_path.clone(),
                },
            )
            .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;

        Ok(StagedBuildResult {
            staged_path: output_path,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InitramfsFunction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InitramfsInput {
    source_root: PathBuf,
    output_path: PathBuf,
}

impl RuntimeFunction for InitramfsFunction {
    type Input = InitramfsInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "initramfs"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        write_initramfs_image(input).map_err(|error| RuntimeError::new(error.to_string()))
    }
}

fn write_initramfs_image(input: InitramfsInput) -> Result<(), InitramfsError> {
    if input.output_path.exists() {
        return Err(InitramfsError::InvalidInput(format!(
            "Initramfs output path already exists: '{}'",
            input.output_path.display()
        )));
    }

    let entries = scan_initramfs_root(&input.source_root)?;
    let output = fs::File::create(&input.output_path).map_err(|error| {
        InitramfsError::Io(format!(
            "failed to create initramfs '{}': {error}",
            input.output_path.display()
        ))
    })?;
    write_newc_initramfs(output, &entries)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NewcEntry {
    Directory {
        path: String,
        uid: u32,
        gid: u32,
        mode: u32,
    },
    File {
        path: String,
        uid: u32,
        gid: u32,
        mode: u32,
        source_path: PathBuf,
    },
    Symlink {
        path: String,
        uid: u32,
        gid: u32,
        target: String,
    },
}

impl NewcEntry {
    fn path(&self) -> &str {
        match self {
            Self::Directory { path, .. } | Self::File { path, .. } | Self::Symlink { path, .. } => {
                path
            }
        }
    }
}

fn scan_initramfs_root(source_root: &Path) -> Result<Vec<NewcEntry>, InitramfsError> {
    require_existing_real_directory(source_root, "initramfs source root")?;

    let mut entries = Vec::new();
    scan_initramfs_entry(source_root, source_root, &mut entries)?;
    entries.sort_by(|left, right| left.path().as_bytes().cmp(right.path().as_bytes()));
    Ok(entries)
}

fn scan_initramfs_entry(
    source_root: &Path,
    path: &Path,
    entries: &mut Vec<NewcEntry>,
) -> Result<(), InitramfsError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        InitramfsError::Io(format!(
            "failed to inspect initramfs entry '{}': {error}",
            path.display()
        ))
    })?;
    let file_type = metadata.file_type();
    let rel_path = manifest_relative_path(source_root, path)?;

    if file_type.is_dir() {
        entries.push(NewcEntry::Directory {
            path: rel_path,
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.permissions().mode() & 0o7777,
        });

        let mut children = fs::read_dir(path)
            .map_err(|error| {
                InitramfsError::Io(format!(
                    "failed to read initramfs directory '{}': {error}",
                    path.display()
                ))
            })?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                InitramfsError::Io(format!(
                    "failed to read initramfs directory entry '{}': {error}",
                    path.display()
                ))
            })?;
        children.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
        for child in children {
            scan_initramfs_entry(source_root, &child, entries)?;
        }
    } else if file_type.is_file() {
        entries.push(NewcEntry::File {
            path: rel_path,
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.permissions().mode() & 0o7777,
            source_path: path.to_path_buf(),
        });
    } else if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|error| {
            InitramfsError::Io(format!(
                "failed to read initramfs symlink '{}': {error}",
                path.display()
            ))
        })?;
        let target = target.to_str().ok_or_else(|| {
            InitramfsError::InvalidInput(format!(
                "initramfs symlink target for '{}' is not UTF-8",
                path.display()
            ))
        })?;
        entries.push(NewcEntry::Symlink {
            path: rel_path,
            uid: metadata.uid(),
            gid: metadata.gid(),
            target: target.to_string(),
        });
    } else {
        return Err(InitramfsError::InvalidInput(format!(
            "unsupported initramfs entry kind at '{}'",
            path.display()
        )));
    }

    Ok(())
}

fn write_newc_initramfs<W: Write>(writer: W, entries: &[NewcEntry]) -> Result<(), InitramfsError> {
    let mut writer = io::BufWriter::new(writer);
    let mut ino = 1_u32;
    for entry in entries {
        write_entry(&mut writer, ino, entry)?;
        ino = ino.checked_add(1).ok_or_else(|| {
            InitramfsError::InvalidInput("initramfs entry inode counter overflowed".to_string())
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
    entry: &NewcEntry,
) -> Result<(), InitramfsError> {
    match entry {
        NewcEntry::Directory {
            path,
            uid,
            gid,
            mode,
        } => write_header_and_payload(
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
        NewcEntry::File {
            path,
            uid,
            gid,
            mode,
            source_path,
        } => {
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
                InitramfsError::InvalidInput(format!(
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
        NewcEntry::Symlink {
            path,
            uid,
            gid,
            target,
        } => {
            let filesize = u32::try_from(target.len()).map_err(|_| {
                InitramfsError::InvalidInput(format!(
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
        InitramfsError::InvalidInput(format!(
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

fn require_existing_real_directory(path: &Path, label: &str) -> Result<(), InitramfsError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        InitramfsError::Io(format!(
            "failed to inspect {label} '{}': {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(InitramfsError::InvalidInput(format!(
            "{label} must be an existing real directory: '{}'",
            path.display()
        )))
    }
}

fn manifest_relative_path(source_root: &Path, path: &Path) -> Result<String, InitramfsError> {
    let relative = path.strip_prefix(source_root).map_err(|error| {
        InitramfsError::InvalidInput(format!(
            "failed to resolve '{}' relative to '{}': {error}",
            path.display(),
            source_root.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(String::new());
    }
    relative.to_str().map(str::to_string).ok_or_else(|| {
        InitramfsError::InvalidInput(format!(
            "initramfs entry path '{}' is not UTF-8",
            path.display()
        ))
    })
}

#[derive(Debug)]
enum InitramfsError {
    InvalidInput(String),
    Io(String),
}

impl fmt::Display for InitramfsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) | Self::Io(message) => formatter.write_str(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Builder, BuilderInputPath};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::tempdir;

    #[test]
    fn input_spec_is_single_fs_tree_root_input() {
        assert_eq!(TypedBuilder::tag(&InitramfsBuilder), "Initramfs");
        assert_eq!(INITRAMFS_SPEC.required_inputs, &["_tree"]);
        assert!(!INITRAMFS_SPEC.allow_extra_inputs);
    }

    #[test]
    fn build_rejects_missing_tree_input() {
        let temp = tempdir().unwrap();
        let mut cx = BuildContext::with_noop_logger(temp.path().join("tmp"));

        let error = InitramfsBuilder
            .build_typed(InitramfsConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();

        assert!(error.to_string().contains("required input slot '_tree'"));
    }

    #[test]
    fn runtime_function_writes_deterministic_newc_image() {
        let temp = tempdir().unwrap();
        let root = sample_root(temp.path());
        let first = temp.path().join("first.img");
        let second = temp.path().join("second.img");

        write_initramfs_image(InitramfsInput {
            source_root: root.clone(),
            output_path: first.clone(),
        })
        .unwrap();
        write_initramfs_image(InitramfsInput {
            source_root: root,
            output_path: second.clone(),
        })
        .unwrap();

        assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
    }

    #[test]
    fn runtime_function_records_file_directory_and_symlink_entries() {
        let temp = tempdir().unwrap();
        let root = sample_root(temp.path());

        let entries = scan_initramfs_root(&root).unwrap();
        let paths = entries.iter().map(NewcEntry::path).collect::<Vec<_>>();

        assert_eq!(paths, vec!["", "bin", "bin/tool", "tool-link"]);
        assert!(matches!(entries[0], NewcEntry::Directory { .. }));
        assert!(matches!(entries[1], NewcEntry::Directory { .. }));
        assert!(matches!(entries[2], NewcEntry::File { .. }));
        assert!(matches!(entries[3], NewcEntry::Symlink { .. }));
    }

    #[test]
    fn writes_expected_newc_archive() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("init");
        fs::write(&file_path, b"#!/bin/sh\n").unwrap();
        let entries = vec![
            NewcEntry::Directory {
                path: String::new(),
                uid: 0,
                gid: 0,
                mode: 0o755,
            },
            NewcEntry::Directory {
                path: "bin".to_string(),
                uid: 1,
                gid: 2,
                mode: 0o755,
            },
            NewcEntry::File {
                path: "init".to_string(),
                uid: 3,
                gid: 4,
                mode: 0o755,
                source_path: file_path,
            },
            NewcEntry::Symlink {
                path: "bin/sh".to_string(),
                uid: 5,
                gid: 6,
                target: "../init".to_string(),
            },
        ];

        let mut first = Vec::new();
        write_newc_initramfs(&mut first, &entries).unwrap();
        let mut second = Vec::new();
        write_newc_initramfs(&mut second, &entries).unwrap();

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
        assert_eq!(parsed[3].mode, S_IFLNK | SYMLINK_MODE);
        assert_eq!(parsed[3].uid, 5);
        assert_eq!(parsed[3].gid, 6);
        assert_eq!(parsed[3].data, b"../init");
    }

    #[test]
    fn runtime_function_rejects_existing_output_path() {
        let temp = tempdir().unwrap();
        let root = sample_root(temp.path());
        let output_path = temp.path().join("initramfs.img");
        fs::write(&output_path, b"already exists").unwrap();

        let error = write_initramfs_image(InitramfsInput {
            source_root: root,
            output_path,
        })
        .unwrap_err();

        assert!(error.to_string().contains("output path already exists"));
    }

    #[test]
    fn build_rejects_unknown_config_field() {
        let error = InitramfsBuilder
            .build_erased(
                serde_json::json!({"extra": true}),
                BuilderInputs::empty(),
                &mut BuildContext::with_noop_logger(PathBuf::from("/tmp/unused")),
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn builder_accepts_tree_input_path_shape() {
        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "_tree",
            BuilderInputPath {
                path: PathBuf::from("/fs-tree-root"),
            },
        );

        assert_eq!(
            inputs.required("_tree").unwrap().path,
            PathBuf::from("/fs-tree-root")
        );
    }

    fn sample_root(parent: &Path) -> PathBuf {
        let root = parent.join("root");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::set_permissions(root.join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("bin/tool"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("bin/tool", root.join("tool-link")).unwrap();
        root
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ParsedEntry {
        name: String,
        mode: u32,
        uid: u32,
        gid: u32,
        mtime: u32,
        data: Vec<u8>,
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
