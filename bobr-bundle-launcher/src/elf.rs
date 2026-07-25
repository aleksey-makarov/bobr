//! Minimal strict ELF64 inspection for HostBundle executable dispatch.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const ELF_HEADER_SIZE: usize = 64;
const ELF64_PROGRAM_HEADER_SIZE: usize = 56;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_INTERP: u32 = 3;
const MAX_INTERPRETER_SIZE: u64 = 4096;

/// Whether an ELF executable requires a userspace interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElfLinkage {
    /// No `PT_INTERP`; the kernel can enter the executable directly.
    Static,
    /// One absolute `PT_INTERP` path naming the dynamic loader.
    Dynamic {
        /// Absolute logical loader path embedded in the payload ELF.
        interpreter: PathBuf,
    },
}

/// Validated ELF properties needed by the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfExecutable {
    linkage: ElfLinkage,
    position_independent: bool,
}

impl ElfExecutable {
    /// Returns the executable's static or dynamic linkage.
    pub fn linkage(&self) -> &ElfLinkage {
        &self.linkage
    }

    /// Returns whether the ELF file type is `ET_DYN`.
    pub fn is_position_independent(&self) -> bool {
        self.position_independent
    }
}

/// Malformed or unsupported ELF executable.
#[derive(Debug)]
pub enum ElfError {
    /// The executable could not be opened or read.
    Read {
        /// Executable path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The file is shorter than a complete ELF64 header.
    TruncatedHeader,
    /// The file does not begin with ELF magic.
    InvalidMagic,
    /// The file is not 64-bit ELF.
    UnsupportedClass(u8),
    /// The file is not little-endian ELF.
    UnsupportedEndian(u8),
    /// The ELF identification or header version is unsupported.
    UnsupportedVersion(u32),
    /// The file targets a machine other than x86-64.
    UnsupportedMachine(u16),
    /// The ELF type is neither executable nor position-independent.
    UnsupportedType(u16),
    /// The ELF header advertises an incompatible header size.
    InvalidHeaderSize(u16),
    /// The program-header entry size is not the ELF64 size.
    InvalidProgramHeaderSize(u16),
    /// The program-header table lies outside the file.
    InvalidProgramHeaderTable,
    /// More than one `PT_INTERP` segment is present.
    DuplicateInterpreter,
    /// `PT_INTERP` is empty, oversized, unterminated, or contains early NUL.
    InvalidInterpreter,
    /// The interpreter path is not absolute.
    RelativeInterpreter(PathBuf),
}

impl ElfError {
    /// Returns whether the failure specifically means the file is not ELF.
    pub fn is_invalid_magic(&self) -> bool {
        matches!(self, Self::InvalidMagic)
    }
}

impl fmt::Display for ElfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read ELF '{}': {source}",
                    path.display()
                )
            }
            Self::TruncatedHeader => formatter.write_str("truncated ELF header"),
            Self::InvalidMagic => formatter.write_str("executable is not ELF"),
            Self::UnsupportedClass(class) => {
                write!(formatter, "unsupported ELF class {class} (expected ELF64)")
            }
            Self::UnsupportedEndian(data) => write!(
                formatter,
                "unsupported ELF data encoding {data} (expected little-endian)"
            ),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported ELF version {version}")
            }
            Self::UnsupportedMachine(machine) => {
                write!(
                    formatter,
                    "unsupported ELF machine {machine} (expected x86-64)"
                )
            }
            Self::UnsupportedType(kind) => write!(formatter, "unsupported ELF type {kind}"),
            Self::InvalidHeaderSize(size) => write!(formatter, "invalid ELF header size {size}"),
            Self::InvalidProgramHeaderSize(size) => {
                write!(formatter, "invalid ELF program-header size {size}")
            }
            Self::InvalidProgramHeaderTable => {
                formatter.write_str("ELF program-header table lies outside the file")
            }
            Self::DuplicateInterpreter => {
                formatter.write_str("ELF contains more than one PT_INTERP")
            }
            Self::InvalidInterpreter => formatter.write_str("ELF contains invalid PT_INTERP"),
            Self::RelativeInterpreter(path) => write!(
                formatter,
                "ELF interpreter '{}' is not absolute",
                path.display()
            ),
        }
    }
}

impl Error for ElfError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Inspects one x86-64 ELF executable and determines its linkage.
pub fn inspect_elf(path: &Path) -> Result<ElfExecutable, ElfError> {
    let file = File::open(path).map_err(|source| ElfError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let file_size = file
        .metadata()
        .map_err(|source| ElfError::Read {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if file_size < ELF_HEADER_SIZE as u64 {
        return Err(ElfError::TruncatedHeader);
    }

    let mut header = [0_u8; ELF_HEADER_SIZE];
    read_exact_at(&file, &mut header, 0, path)?;
    if header[..4] != *b"\x7fELF" {
        return Err(ElfError::InvalidMagic);
    }
    if header[4] != ELFCLASS64 {
        return Err(ElfError::UnsupportedClass(header[4]));
    }
    if header[5] != ELFDATA2LSB {
        return Err(ElfError::UnsupportedEndian(header[5]));
    }
    if header[6] != EV_CURRENT {
        return Err(ElfError::UnsupportedVersion(u32::from(header[6])));
    }

    let elf_type = read_u16(&header, 16);
    if elf_type != ET_EXEC && elf_type != ET_DYN {
        return Err(ElfError::UnsupportedType(elf_type));
    }
    let machine = read_u16(&header, 18);
    if machine != EM_X86_64 {
        return Err(ElfError::UnsupportedMachine(machine));
    }
    let version = read_u32(&header, 20);
    if version != u32::from(EV_CURRENT) {
        return Err(ElfError::UnsupportedVersion(version));
    }
    let header_size = read_u16(&header, 52);
    if usize::from(header_size) != ELF_HEADER_SIZE {
        return Err(ElfError::InvalidHeaderSize(header_size));
    }
    let program_header_size = read_u16(&header, 54);
    if usize::from(program_header_size) != ELF64_PROGRAM_HEADER_SIZE {
        return Err(ElfError::InvalidProgramHeaderSize(program_header_size));
    }

    let program_header_offset = read_u64(&header, 32);
    let program_header_count = u64::from(read_u16(&header, 56));
    let table_size = program_header_count
        .checked_mul(ELF64_PROGRAM_HEADER_SIZE as u64)
        .ok_or(ElfError::InvalidProgramHeaderTable)?;
    let table_end = program_header_offset
        .checked_add(table_size)
        .ok_or(ElfError::InvalidProgramHeaderTable)?;
    if table_end > file_size {
        return Err(ElfError::InvalidProgramHeaderTable);
    }

    let mut table =
        vec![0_u8; usize::try_from(table_size).map_err(|_| ElfError::InvalidProgramHeaderTable)?];
    read_exact_at(&file, &mut table, program_header_offset, path)?;
    let mut interpreter = None;
    for program_header in table.chunks_exact(ELF64_PROGRAM_HEADER_SIZE) {
        if read_u32(program_header, 0) != PT_INTERP {
            continue;
        }
        if interpreter.is_some() {
            return Err(ElfError::DuplicateInterpreter);
        }
        let offset = read_u64(program_header, 8);
        let size = read_u64(program_header, 32);
        if !(2..=MAX_INTERPRETER_SIZE).contains(&size) {
            return Err(ElfError::InvalidInterpreter);
        }
        let end = offset
            .checked_add(size)
            .ok_or(ElfError::InvalidInterpreter)?;
        if end > file_size {
            return Err(ElfError::InvalidInterpreter);
        }
        let mut bytes =
            vec![0_u8; usize::try_from(size).map_err(|_| ElfError::InvalidInterpreter)?];
        read_exact_at(&file, &mut bytes, offset, path)?;
        if bytes.last() != Some(&0) || bytes[..bytes.len() - 1].contains(&0) {
            return Err(ElfError::InvalidInterpreter);
        }
        bytes.pop();
        let path = PathBuf::from(OsString::from_vec(bytes));
        if !path.is_absolute() {
            return Err(ElfError::RelativeInterpreter(path));
        }
        interpreter = Some(path);
    }

    Ok(ElfExecutable {
        linkage: interpreter.map_or(ElfLinkage::Static, |interpreter| ElfLinkage::Dynamic {
            interpreter,
        }),
        position_independent: elf_type == ET_DYN,
    })
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64, path: &Path) -> Result<(), ElfError> {
    file.read_exact_at(buffer, offset)
        .map_err(|source| ElfError::Read {
            path: path.to_path_buf(),
            source,
        })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn elf(elf_type: u16, interpreters: &[&[u8]]) -> Vec<u8> {
        let program_header_offset = ELF_HEADER_SIZE;
        let table_size = interpreters.len() * ELF64_PROGRAM_HEADER_SIZE;
        let mut bytes = vec![0_u8; program_header_offset + table_size];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = ELFCLASS64;
        bytes[5] = ELFDATA2LSB;
        bytes[6] = EV_CURRENT;
        bytes[16..18].copy_from_slice(&elf_type.to_le_bytes());
        bytes[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
        bytes[32..40].copy_from_slice(&(program_header_offset as u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&(ELF_HEADER_SIZE as u16).to_le_bytes());
        bytes[54..56].copy_from_slice(&(ELF64_PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        bytes[56..58].copy_from_slice(&(interpreters.len() as u16).to_le_bytes());

        for (index, interpreter) in interpreters.iter().enumerate() {
            let header_offset = program_header_offset + index * ELF64_PROGRAM_HEADER_SIZE;
            let content_offset = bytes.len();
            bytes[header_offset..header_offset + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
            bytes[header_offset + 8..header_offset + 16]
                .copy_from_slice(&(content_offset as u64).to_le_bytes());
            bytes[header_offset + 32..header_offset + 40]
                .copy_from_slice(&(interpreter.len() as u64).to_le_bytes());
            bytes.extend_from_slice(interpreter);
        }
        bytes
    }

    fn inspect(bytes: &[u8]) -> Result<ElfExecutable, ElfError> {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp.path(), bytes).unwrap();
        inspect_elf(temp.path())
    }

    #[test]
    fn identifies_static_executable() {
        let executable = inspect(&elf(ET_EXEC, &[])).unwrap();

        assert_eq!(executable.linkage(), &ElfLinkage::Static);
        assert!(!executable.is_position_independent());
    }

    #[test]
    fn identifies_static_pie() {
        let executable = inspect(&elf(ET_DYN, &[])).unwrap();

        assert_eq!(executable.linkage(), &ElfLinkage::Static);
        assert!(executable.is_position_independent());
    }

    #[test]
    fn identifies_dynamic_executable_and_interpreter() {
        let executable = inspect(&elf(ET_DYN, &[b"/lib64/ld-linux-x86-64.so.2\0"])).unwrap();

        assert_eq!(
            executable.linkage(),
            &ElfLinkage::Dynamic {
                interpreter: PathBuf::from("/lib64/ld-linux-x86-64.so.2")
            }
        );
    }

    #[test]
    fn identifies_current_test_binary_as_dynamic_elf() {
        let executable = inspect_elf(&std::env::current_exe().unwrap()).unwrap();

        assert!(matches!(executable.linkage(), ElfLinkage::Dynamic { .. }));
    }

    #[test]
    fn rejects_non_elf_and_truncated_header() {
        assert!(matches!(
            inspect(b"not elf"),
            Err(ElfError::TruncatedHeader)
        ));
        let mut not_elf = vec![0_u8; ELF_HEADER_SIZE];
        not_elf[..4].copy_from_slice(b"NOPE");
        assert!(matches!(inspect(&not_elf), Err(ElfError::InvalidMagic)));
    }

    #[test]
    fn rejects_unsupported_class_endian_machine_type_and_version() {
        type ErrorPredicate = fn(&ElfError) -> bool;
        let cases: Vec<(Vec<u8>, ErrorPredicate)> = vec![
            {
                let mut bytes = elf(ET_EXEC, &[]);
                bytes[4] = 1;
                (bytes, |error| {
                    matches!(error, ElfError::UnsupportedClass(1))
                })
            },
            {
                let mut bytes = elf(ET_EXEC, &[]);
                bytes[5] = 2;
                (bytes, |error| {
                    matches!(error, ElfError::UnsupportedEndian(2))
                })
            },
            {
                let mut bytes = elf(ET_EXEC, &[]);
                bytes[18..20].copy_from_slice(&3_u16.to_le_bytes());
                (bytes, |error| {
                    matches!(error, ElfError::UnsupportedMachine(3))
                })
            },
            {
                let bytes = elf(1, &[]);
                (bytes, |error| matches!(error, ElfError::UnsupportedType(1)))
            },
            {
                let mut bytes = elf(ET_EXEC, &[]);
                bytes[20..24].copy_from_slice(&2_u32.to_le_bytes());
                (bytes, |error| {
                    matches!(error, ElfError::UnsupportedVersion(2))
                })
            },
        ];

        for (bytes, matches_expected) in cases {
            let error = inspect(&bytes).unwrap_err();
            assert!(matches_expected(&error), "unexpected error: {error}");
        }
    }

    #[test]
    fn rejects_program_header_table_outside_file() {
        let mut bytes = elf(ET_EXEC, &[]);
        bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());

        assert!(matches!(
            inspect(&bytes),
            Err(ElfError::InvalidProgramHeaderTable)
        ));
    }

    #[test]
    fn rejects_duplicate_interpreter() {
        let bytes = elf(ET_DYN, &[b"/lib64/loader-one\0", b"/lib64/loader-two\0"]);

        assert!(matches!(
            inspect(&bytes),
            Err(ElfError::DuplicateInterpreter)
        ));
    }

    #[test]
    fn rejects_unterminated_and_early_nul_interpreter() {
        assert!(matches!(
            inspect(&elf(ET_DYN, &[b"/lib64/loader"])),
            Err(ElfError::InvalidInterpreter)
        ));
        assert!(matches!(
            inspect(&elf(ET_DYN, &[b"/lib64\0/loader\0"])),
            Err(ElfError::InvalidInterpreter)
        ));
    }

    #[test]
    fn rejects_relative_interpreter() {
        assert!(matches!(
            inspect(&elf(ET_DYN, &[b"lib64/loader\0"])),
            Err(ElfError::RelativeInterpreter(_))
        ));
    }

    #[test]
    fn read_error_reports_path() {
        let path = Path::new("/definitely/missing/elf");

        let error = inspect_elf(path).unwrap_err();

        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.source().is_some());
    }
}
