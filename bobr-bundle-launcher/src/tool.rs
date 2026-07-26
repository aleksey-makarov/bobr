//! Safe resolution of configured tool paths inside a HostBundle payload.

use crate::{BundleConfig, BundleLocation, ToolConfig};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A configured tool whose payload path has been validated and resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    name: String,
    target: PathBuf,
    payload_root: PathBuf,
    config: ToolConfig,
}

impl ResolvedTool {
    /// Returns the key used to select the tool from `bundle.toml`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the canonical path of the executable inside the payload.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Returns the canonical payload root containing the executable.
    pub fn payload_root(&self) -> &Path {
        &self.payload_root
    }

    /// Returns the tool's typed runtime configuration.
    pub fn config(&self) -> &ToolConfig {
        &self.config
    }
}

/// Failure to select or safely resolve a configured tool.
#[derive(Debug)]
pub enum ToolResolutionError {
    /// The requested tool key does not exist.
    UnknownTool(String),
    /// A configured relative path has an unsafe lexical form.
    InvalidRelativePath {
        /// Configuration field containing the path.
        field: String,
        /// Rejected path value.
        value: String,
        /// Specific validation failure.
        reason: &'static str,
    },
    /// A configured path could not be resolved.
    ResolvePath {
        /// Configuration field containing the path.
        field: String,
        /// Host path attempted by the resolver.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The configured payload root resolves outside the HostBundle.
    PayloadEscapesBundle {
        /// Canonical HostBundle root.
        bundle_root: PathBuf,
        /// Canonical escaped payload root.
        payload_root: PathBuf,
    },
    /// The configured tool resolves outside the payload root.
    ToolEscapesPayload {
        /// Tool name from `bundle.toml`.
        tool: String,
        /// Canonical payload root.
        payload_root: PathBuf,
        /// Canonical escaped target.
        target: PathBuf,
    },
    /// The resolved target is not a regular file.
    NotRegularFile {
        /// Tool name from `bundle.toml`.
        tool: String,
        /// Canonical target path.
        target: PathBuf,
    },
    /// The resolved target has no executable bits.
    NotExecutable {
        /// Tool name from `bundle.toml`.
        tool: String,
        /// Canonical target path.
        target: PathBuf,
    },
}

impl fmt::Display for ToolResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTool(tool) => write!(formatter, "unknown bundle tool '{tool}'"),
            Self::InvalidRelativePath {
                field,
                value,
                reason,
            } => write!(
                formatter,
                "invalid bundle-relative path in {field} ('{value}'): {reason}"
            ),
            Self::ResolvePath {
                field,
                path,
                source,
            } => write!(
                formatter,
                "failed to resolve {field} path '{}': {source}",
                path.display()
            ),
            Self::PayloadEscapesBundle {
                bundle_root,
                payload_root,
            } => write!(
                formatter,
                "payload root '{}' resolves outside bundle root '{}'",
                payload_root.display(),
                bundle_root.display()
            ),
            Self::ToolEscapesPayload {
                tool,
                payload_root,
                target,
            } => write!(
                formatter,
                "tool '{tool}' resolves to '{}' outside payload root '{}'",
                target.display(),
                payload_root.display()
            ),
            Self::NotRegularFile { tool, target } => write!(
                formatter,
                "tool '{tool}' target '{}' is not a regular file",
                target.display()
            ),
            Self::NotExecutable { tool, target } => write!(
                formatter,
                "tool '{tool}' target '{}' is not executable",
                target.display()
            ),
        }
    }
}

impl Error for ToolResolutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ResolvePath { source, .. } => Some(source),
            Self::UnknownTool(_)
            | Self::InvalidRelativePath { .. }
            | Self::PayloadEscapesBundle { .. }
            | Self::ToolEscapesPayload { .. }
            | Self::NotRegularFile { .. }
            | Self::NotExecutable { .. } => None,
        }
    }
}

/// Selects `tool_name` and safely resolves its executable inside the payload.
pub fn resolve_tool(
    location: &BundleLocation,
    bundle: &BundleConfig,
    tool_name: &str,
) -> Result<ResolvedTool, ToolResolutionError> {
    let tool = bundle
        .tools
        .get(tool_name)
        .ok_or_else(|| ToolResolutionError::UnknownTool(tool_name.to_string()))?;

    validate_relative_path("payload_root", &bundle.payload_root)?;
    validate_relative_path(&format!("tools.{tool_name}.path"), &tool.path)?;

    let bundle_root = canonicalize(
        "bundle root",
        location.root(),
        location.root().to_path_buf(),
    )?;
    let payload_candidate = bundle_root.join(&bundle.payload_root);
    let payload_root = canonicalize(
        "payload_root",
        &payload_candidate,
        payload_candidate.clone(),
    )?;
    if !payload_root.starts_with(&bundle_root) {
        return Err(ToolResolutionError::PayloadEscapesBundle {
            bundle_root,
            payload_root,
        });
    }

    let target_candidate = bundle_root.join(&tool.path);
    let target = canonicalize(
        &format!("tools.{tool_name}.path"),
        &target_candidate,
        target_candidate.clone(),
    )?;
    if !target.starts_with(&payload_root) {
        return Err(ToolResolutionError::ToolEscapesPayload {
            tool: tool_name.to_string(),
            payload_root,
            target,
        });
    }

    let metadata = fs::metadata(&target).map_err(|source| ToolResolutionError::ResolvePath {
        field: format!("tools.{tool_name}.path"),
        path: target.clone(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ToolResolutionError::NotRegularFile {
            tool: tool_name.to_string(),
            target,
        });
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(ToolResolutionError::NotExecutable {
            tool: tool_name.to_string(),
            target,
        });
    }

    Ok(ResolvedTool {
        name: tool_name.to_string(),
        target,
        payload_root,
        config: tool.clone(),
    })
}

/// Validates a UTF-8 bundle-relative path without touching the filesystem.
pub fn validate_relative_path(field: &str, value: &str) -> Result<(), ToolResolutionError> {
    let invalid = |reason| ToolResolutionError::InvalidRelativePath {
        field: field.to_string(),
        value: value.to_string(),
        reason,
    };
    if value.is_empty() {
        return Err(invalid("path is empty"));
    }
    if value.starts_with('/') {
        return Err(invalid("absolute paths are forbidden"));
    }
    if value.contains('\0') {
        return Err(invalid("NUL bytes are forbidden"));
    }
    if value
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(invalid("empty, '.' and '..' components are forbidden"));
    }
    Ok(())
}

fn canonicalize(
    field: &str,
    path: &Path,
    reported_path: PathBuf,
) -> Result<PathBuf, ToolResolutionError> {
    fs::canonicalize(path).map_err(|source| ToolResolutionError::ResolvePath {
        field: field.to_string(),
        path: reported_path,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BUNDLE_FORMAT_V1, locate_bundle_from_launcher};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        _temp: tempfile::TempDir,
        location: BundleLocation,
        config: BundleConfig,
    }

    impl Fixture {
        fn new(tool_path: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("bundle");
            fs::create_dir_all(root.join("libexec")).unwrap();
            fs::create_dir_all(root.join("root/usr/bin")).unwrap();
            let launcher = root.join("libexec/bobr-bundle-launcher");
            let location = locate_bundle_from_launcher(&launcher).unwrap();
            let config = BundleConfig::parse(&format!(
                r#"
format = "{BUNDLE_FORMAT_V1}"
payload_root = "root"
policy = "strict"
[platform]
os = "linux"
arch = "x86_64"
min_kernel = "4.19"
[loader]
kind = "glibc"
library_dirs = []
inhibit_cache = true
[tools.demo]
path = "{tool_path}"
argv0 = "demo"
visibility = "public"
"#
            ))
            .unwrap();
            Self {
                _temp: temp,
                location,
                config,
            }
        }

        fn root(&self) -> &Path {
            self.location.root()
        }

        fn write_executable(&self, relative: &str) {
            let path = self.root().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"fixture").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn resolves_executable_inside_payload() {
        let fixture = Fixture::new("root/usr/bin/demo");
        fixture.write_executable("root/usr/bin/demo");

        let tool = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap();

        assert_eq!(
            tool.target(),
            fixture.root().join("root/usr/bin/demo").as_path()
        );
        assert_eq!(tool.payload_root(), fixture.root().join("root").as_path());
        assert_eq!(tool.config().argv0, "demo");
    }

    #[test]
    fn follows_relative_symlink_that_stays_inside_payload() {
        let fixture = Fixture::new("root/usr/bin/demo");
        fixture.write_executable("root/usr/libexec/demo-real");
        symlink(
            "../libexec/demo-real",
            fixture.root().join("root/usr/bin/demo"),
        )
        .unwrap();

        let tool = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap();

        assert_eq!(
            tool.target(),
            fixture.root().join("root/usr/libexec/demo-real").as_path()
        );
    }

    #[test]
    fn rejects_unknown_tool() {
        let fixture = Fixture::new("root/usr/bin/demo");

        let error = resolve_tool(&fixture.location, &fixture.config, "missing").unwrap_err();

        assert!(matches!(error, ToolResolutionError::UnknownTool(_)));
    }

    #[test]
    fn rejects_unsafe_lexical_tool_paths() {
        for path in [
            "",
            "/usr/bin/demo",
            "../demo",
            "root/../demo",
            "root/./demo",
            "root//demo",
            "root/demo/",
        ] {
            let fixture = Fixture::new(path);
            let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();
            assert!(
                matches!(error, ToolResolutionError::InvalidRelativePath { .. }),
                "path {path:?} produced {error:?}"
            );
        }
    }

    #[test]
    fn rejects_payload_root_with_unsafe_lexical_path() {
        let mut fixture = Fixture::new("root/usr/bin/demo");
        fixture.config.payload_root = "../root".to_string();

        let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();

        assert!(matches!(
            error,
            ToolResolutionError::InvalidRelativePath { field, .. } if field == "payload_root"
        ));
    }

    #[test]
    fn rejects_tool_symlink_that_escapes_payload() {
        let fixture = Fixture::new("root/usr/bin/demo");
        fixture.write_executable("outside");
        symlink("../../../outside", fixture.root().join("root/usr/bin/demo")).unwrap();

        let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();

        assert!(matches!(
            error,
            ToolResolutionError::ToolEscapesPayload { .. }
        ));
    }

    #[test]
    fn rejects_absolute_tool_symlink_to_host() {
        let fixture = Fixture::new("root/usr/bin/demo");
        symlink(
            std::env::current_exe().unwrap(),
            fixture.root().join("root/usr/bin/demo"),
        )
        .unwrap();

        let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();

        assert!(matches!(
            error,
            ToolResolutionError::ToolEscapesPayload { .. }
        ));
    }

    #[test]
    fn rejects_payload_symlink_that_escapes_bundle() {
        let mut fixture = Fixture::new("root/usr/bin/demo");
        fs::remove_dir_all(fixture.root().join("root")).unwrap();
        symlink("/tmp", fixture.root().join("payload")).unwrap();
        fixture.config.payload_root = "payload".to_string();
        fixture.config.tools.get_mut("demo").unwrap().path = "payload/demo".to_string();

        let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();

        assert!(matches!(
            error,
            ToolResolutionError::PayloadEscapesBundle { .. }
        ));
    }

    #[test]
    fn rejects_directory_target() {
        let fixture = Fixture::new("root/usr/bin");

        let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();

        assert!(matches!(error, ToolResolutionError::NotRegularFile { .. }));
    }

    #[test]
    fn rejects_non_executable_target() {
        let fixture = Fixture::new("root/usr/bin/demo");
        fs::write(fixture.root().join("root/usr/bin/demo"), b"fixture").unwrap();

        let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();

        assert!(matches!(error, ToolResolutionError::NotExecutable { .. }));
    }

    #[test]
    fn reports_missing_target_path() {
        let fixture = Fixture::new("root/usr/bin/missing");

        let error = resolve_tool(&fixture.location, &fixture.config, "demo").unwrap_err();

        assert!(matches!(
            error,
            ToolResolutionError::ResolvePath { ref field, .. }
                if field == "tools.demo.path"
        ));
        assert!(error.source().is_some());
    }
}
