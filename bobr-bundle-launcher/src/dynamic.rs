//! Bundled glibc loader resolution and command planning.

use crate::{
    BundleConfig, BundleLocation, ElfError, ElfLinkage, LoaderKind, ResolvedTool, inspect_elf,
};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A complete bundled-loader command prepared for `execve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicLaunchPlan {
    loader: PathBuf,
    arguments: Vec<OsString>,
}

impl DynamicLaunchPlan {
    /// Returns the canonical bundled loader path.
    pub fn loader(&self) -> &Path {
        &self.loader
    }

    /// Returns loader arguments, excluding the loader's own `argv[0]`.
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

/// Failure to map or validate a dynamic ELF launch inside its payload.
#[derive(Debug)]
pub enum DynamicLaunchError {
    /// `PT_INTERP` contains unsafe path components.
    InvalidInterpreter(PathBuf),
    /// The loader or a library directory could not be resolved.
    ResolvePath {
        /// Logical role of the failing path.
        field: String,
        /// Host path attempted by the resolver.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A resolved loader or library path escaped the payload.
    EscapesPayload {
        /// Logical role of the path.
        field: String,
        /// Canonical payload root.
        payload_root: PathBuf,
        /// Canonical escaped path.
        path: PathBuf,
    },
    /// The loader is not a regular executable file.
    InvalidLoaderFile(PathBuf),
    /// The loader is not a supported static-linkage ELF.
    InvalidLoaderElf {
        /// Canonical loader path.
        path: PathBuf,
        /// ELF inspection failure.
        source: ElfError,
    },
    /// The loader unexpectedly requests another interpreter.
    ChainedLoader(PathBuf),
    /// No library search directories were configured.
    EmptyLibraryPath,
    /// A configured library search entry is not a directory.
    LibraryPathNotDirectory(PathBuf),
    /// A loader argument path contains `:`.
    PathContainsSeparator(PathBuf),
}

impl fmt::Display for DynamicLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInterpreter(path) => {
                write!(formatter, "invalid ELF interpreter '{}'", path.display())
            }
            Self::ResolvePath {
                field,
                path,
                source,
            } => write!(
                formatter,
                "failed to resolve {field} '{}': {source}",
                path.display()
            ),
            Self::EscapesPayload {
                field,
                payload_root,
                path,
            } => write!(
                formatter,
                "{field} '{}' resolves outside payload root '{}'",
                path.display(),
                payload_root.display()
            ),
            Self::InvalidLoaderFile(path) => write!(
                formatter,
                "bundled loader '{}' is not an executable regular file",
                path.display()
            ),
            Self::InvalidLoaderElf { path, source } => write!(
                formatter,
                "bundled loader '{}' is not a supported ELF: {source}",
                path.display()
            ),
            Self::ChainedLoader(path) => write!(
                formatter,
                "bundled loader '{}' unexpectedly has PT_INTERP",
                path.display()
            ),
            Self::EmptyLibraryPath => {
                formatter.write_str("dynamic bundle tool has no loader library_dirs")
            }
            Self::LibraryPathNotDirectory(path) => write!(
                formatter,
                "loader library path '{}' is not a directory",
                path.display()
            ),
            Self::PathContainsSeparator(path) => write!(
                formatter,
                "loader path '{}' contains ':' and cannot form --library-path",
                path.display()
            ),
        }
    }
}

impl Error for DynamicLaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ResolvePath { source, .. } => Some(source),
            Self::InvalidLoaderElf { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Maps a dynamic ELF to its bundled glibc loader and arguments.
pub fn prepare_dynamic_launch(
    location: &BundleLocation,
    bundle: &BundleConfig,
    tool: &ResolvedTool,
    interpreter: &Path,
    payload_arguments: &[OsString],
) -> Result<DynamicLaunchPlan, DynamicLaunchError> {
    match bundle.loader.kind {
        LoaderKind::Glibc => {}
    }
    let relative_interpreter = validate_interpreter(interpreter)
        .ok_or_else(|| DynamicLaunchError::InvalidInterpreter(interpreter.to_path_buf()))?;
    let loader_candidate = tool.payload_root().join(relative_interpreter);
    let loader = resolve_inside_payload("ELF interpreter", &loader_candidate, tool.payload_root())?;
    let metadata = fs::metadata(&loader).map_err(|source| DynamicLaunchError::ResolvePath {
        field: "ELF interpreter".to_string(),
        path: loader.clone(),
        source,
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(DynamicLaunchError::InvalidLoaderFile(loader));
    }
    let loader_elf =
        inspect_elf(&loader).map_err(|source| DynamicLaunchError::InvalidLoaderElf {
            path: loader.clone(),
            source,
        })?;
    if !matches!(loader_elf.linkage(), ElfLinkage::Static) {
        return Err(DynamicLaunchError::ChainedLoader(loader));
    }

    if bundle.loader.library_dirs.is_empty() {
        return Err(DynamicLaunchError::EmptyLibraryPath);
    }
    let bundle_root =
        fs::canonicalize(location.root()).map_err(|source| DynamicLaunchError::ResolvePath {
            field: "bundle root".to_string(),
            path: location.root().to_path_buf(),
            source,
        })?;
    let mut library_dirs = Vec::with_capacity(bundle.loader.library_dirs.len());
    for (index, relative) in bundle.loader.library_dirs.iter().enumerate() {
        crate::tool::validate_relative_path(&format!("loader.library_dirs[{index}]"), relative)
            .map_err(|_| DynamicLaunchError::ResolvePath {
                field: format!("loader.library_dirs[{index}]"),
                path: bundle_root.join(relative),
                source: io::Error::new(io::ErrorKind::InvalidInput, "invalid relative path"),
            })?;
        let directory = resolve_inside_payload(
            &format!("loader.library_dirs[{index}]"),
            &bundle_root.join(relative),
            tool.payload_root(),
        )?;
        if !directory.is_dir() {
            return Err(DynamicLaunchError::LibraryPathNotDirectory(directory));
        }
        ensure_no_separator(&directory)?;
        library_dirs.push(directory.into_os_string());
    }

    let mut arguments = Vec::new();
    if bundle.loader.inhibit_cache {
        arguments.push(OsString::from("--inhibit-cache"));
    }
    arguments.push(OsString::from("--argv0"));
    arguments.push(OsString::from(&tool.config().argv0));
    arguments.push(OsString::from("--library-path"));
    arguments.push(join_paths(&library_dirs));
    arguments.push(tool.target().as_os_str().to_os_string());
    arguments.extend_from_slice(payload_arguments);

    Ok(DynamicLaunchPlan { loader, arguments })
}

fn validate_interpreter(path: &Path) -> Option<&Path> {
    let bytes = path.as_os_str().as_bytes();
    if !bytes.starts_with(b"/") {
        return None;
    }
    let relative = &bytes[1..];
    if relative.is_empty()
        || relative
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return None;
    }
    Some(Path::new(OsStr::from_bytes(relative)))
}

fn resolve_inside_payload(
    field: &str,
    candidate: &Path,
    payload_root: &Path,
) -> Result<PathBuf, DynamicLaunchError> {
    let resolved =
        fs::canonicalize(candidate).map_err(|source| DynamicLaunchError::ResolvePath {
            field: field.to_string(),
            path: candidate.to_path_buf(),
            source,
        })?;
    if !resolved.starts_with(payload_root) {
        return Err(DynamicLaunchError::EscapesPayload {
            field: field.to_string(),
            payload_root: payload_root.to_path_buf(),
            path: resolved,
        });
    }
    Ok(resolved)
}

fn ensure_no_separator(path: &Path) -> Result<(), DynamicLaunchError> {
    if path.as_os_str().as_bytes().contains(&b':') {
        return Err(DynamicLaunchError::PathContainsSeparator(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

fn join_paths(paths: &[OsString]) -> OsString {
    let mut bytes = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if index != 0 {
            bytes.push(b':');
        }
        bytes.extend_from_slice(path.as_os_str().as_bytes());
    }
    OsString::from_vec(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_absolute_interpreter_without_special_components() {
        assert_eq!(
            validate_interpreter(Path::new("/lib64/ld-linux.so")),
            Some(Path::new("lib64/ld-linux.so"))
        );
        for invalid in [
            "lib64/ld.so",
            "/",
            "//lib/ld.so",
            "/lib//ld.so",
            "/lib/./ld.so",
            "/lib/../ld.so",
            "/lib/ld.so/",
        ] {
            assert_eq!(validate_interpreter(Path::new(invalid)), None);
        }
    }

    #[test]
    fn joins_library_paths_without_utf8_conversion() {
        let paths = vec![
            OsString::from_vec(vec![b'/', b'a', 0xff]),
            OsString::from("/b"),
        ];

        assert_eq!(
            join_paths(&paths),
            OsString::from_vec(vec![b'/', b'a', 0xff, b':', b'/', b'b'])
        );
    }
}
