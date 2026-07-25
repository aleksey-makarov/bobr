//! Strict runtime configuration for a HostBundle.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Runtime configuration format understood by this launcher.
pub const BUNDLE_FORMAT_V1: &str = "bobr-host-bundle-v1";

/// Complete contents of `bundle.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleConfig {
    format: String,
    /// Bundle-relative directory containing the preserved root filesystem.
    pub payload_root: String,
    /// Host integration policy applied to inherited environment variables.
    pub policy: HostPolicy,
    /// Operating system and architecture required by the payload.
    pub platform: PlatformConfig,
    /// Dynamic loader family and library search configuration.
    pub loader: LoaderConfig,
    /// Common environment operations keyed by variable name.
    #[serde(default)]
    pub environment: BTreeMap<String, EnvironmentRule>,
    /// Public and internal tools addressable through launcher wrappers.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolConfig>,
}

impl BundleConfig {
    /// Parses and validates one v1 runtime configuration document.
    pub fn parse(contents: &str) -> Result<Self, BundleConfigError> {
        let config = toml::from_str::<Self>(contents).map_err(BundleConfigError::Parse)?;
        if config.format != BUNDLE_FORMAT_V1 {
            return Err(BundleConfigError::UnsupportedFormat(config.format));
        }
        if config.tools.is_empty() {
            return Err(BundleConfigError::NoTools);
        }
        Ok(config)
    }

    /// Returns the exact runtime format identifier from the parsed document.
    pub fn format(&self) -> &str {
        &self.format
    }
}

/// Whether the launcher isolates the payload or integrates selected host data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostPolicy {
    /// Reject implicit host library, plugin, and data fallbacks.
    Strict,
    /// Inherit only explicitly configured host integration paths.
    Integrated,
}

/// Platform tuple required by the HostBundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformConfig {
    /// Required operating system.
    pub os: PlatformOs,
    /// Required machine architecture.
    pub arch: PlatformArch,
    /// Minimum supported Linux kernel version.
    pub min_kernel: String,
}

/// Operating systems supported by HostBundle v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformOs {
    /// Linux userspace and procfs semantics.
    Linux,
}

/// Machine architectures supported by HostBundle v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformArch {
    /// 64-bit x86 System V ABI.
    X86_64,
}

/// Runtime dynamic-loader configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoaderConfig {
    /// Loader implementation whose command-line interface is expected.
    pub kind: LoaderKind,
    /// Ordered bundle-relative library search directories.
    pub library_dirs: Vec<String>,
    /// Whether glibc's loader cache is disabled.
    pub inhibit_cache: bool,
}

/// Dynamic-loader implementations supported by HostBundle v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoaderKind {
    /// GNU libc `ld.so`.
    Glibc,
}

/// One named payload program exposed through the launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    /// Bundle-relative path of the real executable.
    pub path: String,
    /// Logical `argv[0]` passed to the payload.
    pub argv0: String,
    /// Environment operations applied after the common environment.
    #[serde(default)]
    pub environment: BTreeMap<String, EnvironmentRule>,
}

/// One typed operation applied to a process environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRule {
    /// How bundle paths and the inherited value are combined.
    pub operation: EnvironmentOperation,
    /// Ordered bundle-relative path entries.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Whether an existing host value participates in the operation.
    #[serde(default)]
    pub inherit: bool,
    /// Values used only when the host variable is absent.
    #[serde(default)]
    pub host_default: Vec<String>,
}

/// Supported typed environment operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvironmentOperation {
    /// Use only the configured values.
    Replace,
    /// Place configured values before an inherited value.
    Prepend,
    /// Place configured values after an inherited value.
    Append,
    /// Remove the variable.
    Unset,
    /// Set the configured value only when the variable is absent.
    Default,
}

/// Failure to read or parse `bundle.toml`.
#[derive(Debug)]
pub enum BundleConfigError {
    /// The configuration file could not be read.
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// TOML syntax or its typed shape was invalid.
    Parse(toml::de::Error),
    /// The document requests an unsupported runtime format.
    UnsupportedFormat(String),
    /// The document contains no addressable tools.
    NoTools,
}

impl fmt::Display for BundleConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read bundle config '{}': {source}",
                    path.display()
                )
            }
            Self::Parse(error) => write!(formatter, "invalid bundle config: {error}"),
            Self::UnsupportedFormat(format) => write!(
                formatter,
                "unsupported bundle config format '{format}' (expected '{BUNDLE_FORMAT_V1}')"
            ),
            Self::NoTools => formatter.write_str("bundle config defines no tools"),
        }
    }
}

impl Error for BundleConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse(error) => Some(error),
            Self::UnsupportedFormat(_) | Self::NoTools => None,
        }
    }
}

/// Reads and strictly parses `bundle.toml` from `path`.
pub fn read_bundle_config(path: &Path) -> Result<BundleConfig, BundleConfigError> {
    let bytes = fs::read_to_string(path).map_err(|source| BundleConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    BundleConfig::parse(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const COMPLETE_CONFIG: &str = r#"
format = "bobr-host-bundle-v1"
payload_root = "root"
policy = "strict"

[platform]
os = "linux"
arch = "x86_64"
min_kernel = "4.19"

[loader]
kind = "glibc"
library_dirs = ["root/usr/lib64", "root/usr/lib"]
inhibit_cache = true

[environment.PATH]
operation = "prepend"
paths = ["libexec/wrapped-bin"]
inherit = true

[environment.XDG_DATA_DIRS]
operation = "prepend"
paths = ["root/usr/share"]
inherit = true
host_default = ["/usr/local/share", "/usr/share"]

[environment.LIBGL_DRIVERS_PATH]
operation = "replace"
paths = ["root/usr/lib/dri"]

[tools.qemu-system-x86_64]
path = "root/usr/bin/qemu-system-x86_64"
argv0 = "qemu-system-x86_64"

[tools.qemu-system-x86_64.environment.QEMU_AUDIO_DRV]
operation = "default"
paths = ["none"]

[tools.qemu-img]
path = "root/usr/bin/qemu-img"
argv0 = "qemu-img"
"#;

    #[test]
    fn parses_complete_v1_config() {
        let config = BundleConfig::parse(COMPLETE_CONFIG).unwrap();

        assert_eq!(config.format(), BUNDLE_FORMAT_V1);
        assert_eq!(config.payload_root, "root");
        assert_eq!(config.policy, HostPolicy::Strict);
        assert_eq!(config.platform.arch, PlatformArch::X86_64);
        assert_eq!(config.loader.kind, LoaderKind::Glibc);
        assert!(config.loader.inhibit_cache);
        assert_eq!(config.environment.len(), 3);
        assert_eq!(config.tools.len(), 2);
        assert_eq!(
            config.tools["qemu-system-x86_64"].environment["QEMU_AUDIO_DRV"].operation,
            EnvironmentOperation::Default
        );
    }

    #[test]
    fn defaults_optional_environment_maps_and_rule_fields() {
        let config = BundleConfig::parse(
            r#"
format = "bobr-host-bundle-v1"
payload_root = "root"
policy = "integrated"
[platform]
os = "linux"
arch = "x86_64"
min_kernel = "4.19"
[loader]
kind = "glibc"
library_dirs = []
inhibit_cache = false
[tools.echo]
path = "root/usr/bin/echo"
argv0 = "echo"
"#,
        )
        .unwrap();

        assert!(config.environment.is_empty());
        assert!(config.tools["echo"].environment.is_empty());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let error = BundleConfig::parse(&format!("{COMPLETE_CONFIG}\nextra = true\n")).unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_unknown_nested_field() {
        let invalid = COMPLETE_CONFIG.replace(
            "min_kernel = \"4.19\"",
            "min_kernel = \"4.19\"\nvariant = \"gnu\"",
        );

        let error = BundleConfig::parse(&invalid).unwrap_err();

        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("variant"));
    }

    #[test]
    fn rejects_unknown_tool_field() {
        let invalid = COMPLETE_CONFIG.replace(
            "argv0 = \"qemu-img\"",
            "argv0 = \"qemu-img\"\nworking_directory = \"root\"",
        );

        let error = BundleConfig::parse(&invalid).unwrap_err();

        assert!(error.to_string().contains("working_directory"));
    }

    #[test]
    fn rejects_unknown_environment_rule_field() {
        let invalid = COMPLETE_CONFIG.replace(
            "paths = [\"root/usr/lib/dri\"]",
            "paths = [\"root/usr/lib/dri\"]\nseparator = \":\"",
        );

        let error = BundleConfig::parse(&invalid).unwrap_err();

        assert!(error.to_string().contains("separator"));
    }

    #[test]
    fn rejects_unsupported_format() {
        let invalid = COMPLETE_CONFIG.replace(BUNDLE_FORMAT_V1, "bobr-host-bundle-v999");

        let error = BundleConfig::parse(&invalid).unwrap_err();

        assert!(matches!(
            error,
            BundleConfigError::UnsupportedFormat(format)
                if format == "bobr-host-bundle-v999"
        ));
    }

    #[test]
    fn rejects_unknown_enum_value() {
        let invalid = COMPLETE_CONFIG.replace("policy = \"strict\"", "policy = \"permissive\"");

        let error = BundleConfig::parse(&invalid).unwrap_err();

        assert!(error.to_string().contains("permissive"));
    }

    #[test]
    fn rejects_missing_required_field() {
        let invalid = COMPLETE_CONFIG.replace("payload_root = \"root\"\n", "");

        let error = BundleConfig::parse(&invalid).unwrap_err();

        assert!(error.to_string().contains("payload_root"));
    }

    #[test]
    fn rejects_config_without_tools() {
        let invalid = COMPLETE_CONFIG
            .split("[tools.qemu-system-x86_64]")
            .next()
            .unwrap();

        let error = BundleConfig::parse(invalid).unwrap_err();

        assert!(matches!(error, BundleConfigError::NoTools));
    }

    #[test]
    fn reads_config_from_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bundle.toml");
        fs::write(&path, COMPLETE_CONFIG).unwrap();

        let config = read_bundle_config(&path).unwrap();

        assert_eq!(config.format(), BUNDLE_FORMAT_V1);
    }

    #[test]
    fn read_error_reports_config_path() {
        let path = Path::new("/definitely/missing/bundle.toml");

        let error = read_bundle_config(path).unwrap_err();

        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.source().is_some());
    }
}
