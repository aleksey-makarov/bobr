//! Strict executable-format and shebang inspection.

use crate::{ElfError, ElfExecutable, PlatformArch, inspect_elf_for_arch};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

const SHEBANG_BUFFER_SIZE: usize = 256;

/// The executable format selected without asking the host kernel to guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutableFormat {
    /// A validated x86-64 ELF executable.
    Elf(ElfExecutable),
    /// A script with a validated absolute shebang interpreter.
    Script(Shebang),
}

/// Parsed kernel-style shebang fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shebang {
    interpreter: PathBuf,
    argument: Option<OsString>,
}

impl Shebang {
    /// Returns the absolute logical interpreter path from the script.
    pub fn interpreter(&self) -> &Path {
        &self.interpreter
    }

    /// Returns the entire optional shebang argument as one argument.
    pub fn argument(&self) -> Option<&OsStr> {
        self.argument.as_deref()
    }
}

/// Failure to identify an executable as a supported ELF or script.
#[derive(Debug)]
pub enum ExecutableInspectionError {
    /// The executable prefix could not be read.
    Read {
        /// Executable path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A file with ELF magic is malformed or unsupported.
    Elf(ElfError),
    /// The file is neither ELF nor a shebang script.
    UnknownFormat,
    /// The shebang line exceeds the kernel-sized inspection buffer.
    TruncatedShebang,
    /// The shebang contains no interpreter.
    MissingInterpreter,
    /// The shebang interpreter is not an absolute safe payload path.
    InvalidInterpreter(PathBuf),
    /// A NUL byte appears in the shebang line.
    ShebangContainsNul,
}

impl fmt::Display for ExecutableInspectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to inspect executable '{}': {source}",
                    path.display()
                )
            }
            Self::Elf(error) => write!(formatter, "invalid ELF executable: {error}"),
            Self::UnknownFormat => {
                formatter.write_str("unknown executable format (expected ELF or shebang)")
            }
            Self::TruncatedShebang => write!(
                formatter,
                "shebang line does not terminate within {SHEBANG_BUFFER_SIZE} bytes"
            ),
            Self::MissingInterpreter => formatter.write_str("shebang has no interpreter"),
            Self::InvalidInterpreter(path) => write!(
                formatter,
                "shebang interpreter '{}' is not an absolute safe payload path",
                path.display()
            ),
            Self::ShebangContainsNul => formatter.write_str("shebang contains a NUL byte"),
        }
    }
}

impl Error for ExecutableInspectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Elf(error) => Some(error),
            _ => None,
        }
    }
}

/// Identifies an executable without falling back to host format handling.
pub fn inspect_executable(path: &Path) -> Result<ExecutableFormat, ExecutableInspectionError> {
    inspect_executable_for_arch(path, PlatformArch::X86_64)
}

/// Identifies an executable for the architecture declared by its bundle.
pub fn inspect_executable_for_arch(
    path: &Path,
    expected_arch: PlatformArch,
) -> Result<ExecutableFormat, ExecutableInspectionError> {
    let mut file = File::open(path).map_err(|source| ExecutableInspectionError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut prefix = Vec::with_capacity(SHEBANG_BUFFER_SIZE);
    file.by_ref()
        .take(SHEBANG_BUFFER_SIZE as u64)
        .read_to_end(&mut prefix)
        .map_err(|source| ExecutableInspectionError::Read {
            path: path.to_path_buf(),
            source,
        })?;

    if prefix.starts_with(b"\x7fELF") {
        return inspect_elf_for_arch(path, expected_arch)
            .map(ExecutableFormat::Elf)
            .map_err(ExecutableInspectionError::Elf);
    }
    if prefix.starts_with(b"#!") {
        return parse_shebang(&prefix).map(ExecutableFormat::Script);
    }
    Err(ExecutableInspectionError::UnknownFormat)
}

fn parse_shebang(prefix: &[u8]) -> Result<Shebang, ExecutableInspectionError> {
    debug_assert!(prefix.starts_with(b"#!"));
    let newline = prefix.iter().position(|byte| *byte == b'\n');
    let end = newline.unwrap_or(prefix.len());
    let buffer_is_full = newline.is_none() && prefix.len() == SHEBANG_BUFFER_SIZE;
    let raw_line = &prefix[2..end];
    if raw_line.contains(&0) {
        return Err(ExecutableInspectionError::ShebangContainsNul);
    }
    if buffer_is_full {
        return Err(ExecutableInspectionError::TruncatedShebang);
    }
    let line = trim_ascii_space_tab(raw_line);
    if line.is_empty() {
        return Err(ExecutableInspectionError::MissingInterpreter);
    }

    let interpreter_end = line
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'))
        .unwrap_or(line.len());
    let interpreter = PathBuf::from(OsString::from_vec(line[..interpreter_end].to_vec()));
    if crate::dynamic::validate_absolute_payload_path(&interpreter).is_none() {
        return Err(ExecutableInspectionError::InvalidInterpreter(interpreter));
    }
    let argument = trim_ascii_space_tab(&line[interpreter_end..]);
    let argument = (!argument.is_empty()).then(|| OsString::from_vec(argument.to_vec()));

    Ok(Shebang {
        interpreter,
        argument,
    })
}

fn trim_ascii_space_tab(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    fn inspect(contents: &[u8]) -> Result<ExecutableFormat, ExecutableInspectionError> {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp.path(), contents).unwrap();
        inspect_executable(temp.path())
    }

    #[test]
    fn parses_interpreter_and_one_optional_argument() {
        let format = inspect(b"#! \t/usr/bin/env \t-S helper --flag  \nbody").unwrap();
        let ExecutableFormat::Script(shebang) = format else {
            panic!("expected script");
        };

        assert_eq!(shebang.interpreter(), Path::new("/usr/bin/env"));
        assert_eq!(shebang.argument().unwrap().as_bytes(), b"-S helper --flag");
    }

    #[test]
    fn accepts_a_final_line_without_newline() {
        let format = inspect(b"#!/bin/sh").unwrap();
        let ExecutableFormat::Script(shebang) = format else {
            panic!("expected script");
        };

        assert_eq!(shebang.interpreter(), Path::new("/bin/sh"));
        assert_eq!(shebang.argument(), None);
    }

    #[test]
    fn rejects_unknown_malformed_and_unsafe_scripts() {
        assert!(matches!(
            inspect(b"ordinary text"),
            Err(ExecutableInspectionError::UnknownFormat)
        ));
        assert!(matches!(
            inspect(b"#! \t \n"),
            Err(ExecutableInspectionError::MissingInterpreter)
        ));
        assert!(matches!(
            inspect(b"#!bin/sh\n"),
            Err(ExecutableInspectionError::InvalidInterpreter(_))
        ));
        assert!(matches!(
            inspect(b"#!/bin/../sh\n"),
            Err(ExecutableInspectionError::InvalidInterpreter(_))
        ));
        assert!(matches!(
            inspect(b"#!/bin/sh\0bad\n"),
            Err(ExecutableInspectionError::ShebangContainsNul)
        ));
    }

    #[test]
    fn rejects_shebang_that_fills_the_inspection_buffer() {
        let mut contents = b"#!/".to_vec();
        contents.resize(SHEBANG_BUFFER_SIZE, b'x');

        assert!(matches!(
            inspect(&contents),
            Err(ExecutableInspectionError::TruncatedShebang)
        ));
    }

    #[test]
    fn rejects_a_full_buffer_even_when_only_the_argument_is_truncated() {
        let mut contents = b"#!/bin/interpreter argument".to_vec();
        contents.resize(SHEBANG_BUFFER_SIZE, b'x');
        assert!(matches!(
            inspect(&contents),
            Err(ExecutableInspectionError::TruncatedShebang)
        ));
    }

    #[test]
    fn preserves_non_utf8_shebang_bytes() {
        let format = inspect(b"#!/bin/i\xff arg\xfe\n").unwrap();
        let ExecutableFormat::Script(shebang) = format else {
            panic!("expected script");
        };

        assert_eq!(shebang.interpreter().as_os_str().as_bytes(), b"/bin/i\xff");
        assert_eq!(shebang.argument().unwrap().as_bytes(), b"arg\xfe");
    }
}
