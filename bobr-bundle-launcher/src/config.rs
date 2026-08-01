//! Strict runtime configuration for a HostBundle.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::platform::KernelVersion;

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
    /// Constructs and validates the current runtime configuration format.
    ///
    /// The caller supplies semantic runtime fields; the format identifier is
    /// owned by the launcher and cannot be selected by a recipe.
    pub fn new_v1(
        payload_root: impl Into<String>,
        policy: HostPolicy,
        platform: PlatformConfig,
        loader: LoaderConfig,
        environment: BTreeMap<String, EnvironmentRule>,
        tools: BTreeMap<String, ToolConfig>,
    ) -> Result<Self, BundleConfigError> {
        let config = Self {
            format: BUNDLE_FORMAT_V1.to_string(),
            payload_root: payload_root.into(),
            policy,
            platform,
            loader,
            environment,
            tools,
        };
        config.validate()?;
        Ok(config)
    }

    /// Parses and validates one v1 runtime configuration document.
    pub fn parse(contents: &str) -> Result<Self, BundleConfigError> {
        let config = toml::from_str::<Self>(contents).map_err(BundleConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates semantic invariants independent of the bundle filesystem.
    pub fn validate(&self) -> Result<(), BundleConfigError> {
        if self.format != BUNDLE_FORMAT_V1 {
            return Err(BundleConfigError::UnsupportedFormat(self.format.clone()));
        }
        if self.tools.is_empty() {
            return Err(BundleConfigError::NoTools);
        }
        KernelVersion::parse_required(&self.platform.min_kernel).map_err(|reason| {
            BundleConfigError::InvalidMinimumKernel {
                value: self.platform.min_kernel.clone(),
                reason,
            }
        })?;
        for name in self.tools.keys() {
            if name == crate::LAUNCHER_BINARY_NAME {
                return Err(BundleConfigError::ReservedToolName(name.clone()));
            }
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.as_bytes().contains(&b'/')
                || name.as_bytes().contains(&b'\0')
            {
                return Err(BundleConfigError::InvalidToolName(name.clone()));
            }
        }
        Ok(())
    }

    /// Serializes the configuration as deterministic TOML with a final newline.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        let mut contents = toml::to_string_pretty(self)?;
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        Ok(contents)
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
    /// 64-bit Arm A-profile ABI.
    Aarch64,
}

impl PlatformArch {
    /// Returns the canonical HostBundle spelling of this architecture.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// Returns the ELF `e_machine` value required for this architecture.
    pub const fn elf_machine(self) -> u16 {
        match self {
            Self::X86_64 => 62,
            Self::Aarch64 => 183,
        }
    }

    /// Returns the architecture of the currently compiled launcher, if supported.
    pub const fn current() -> Option<Self> {
        if cfg!(target_arch = "x86_64") {
            Some(Self::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Self::Aarch64)
        } else {
            None
        }
    }
}

impl fmt::Display for PlatformArch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
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
    /// Whether a wrapper is part of the public interface or only for children.
    pub visibility: ToolVisibility,
    /// Environment operations applied after the common environment.
    #[serde(default)]
    pub environment: BTreeMap<String, EnvironmentRule>,
}

/// How a configured tool is exposed by the HostBundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolVisibility {
    /// The tool has an entry in top-level `bin/`.
    Public,
    /// The tool is reachable only through `libexec/wrapped-bin/`.
    Internal,
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
    /// Ordered literal entries, used without bundle-path resolution.
    #[serde(default)]
    pub values: Vec<String>,
    /// Whether an existing host value participates in the operation.
    #[serde(default)]
    pub inherit: bool,
    /// Values used only when the host variable is absent.
    #[serde(default)]
    pub host_default: Vec<String>,
}

impl EnvironmentRule {
    /// Validates the variable name and operation-specific field combinations.
    pub fn validate(&self, variable: &str) -> Result<(), EnvironmentRuleValidationError> {
        if variable.is_empty()
            || variable.as_bytes().contains(&b'=')
            || variable.as_bytes().contains(&b'\0')
        {
            return Err(EnvironmentRuleValidationError::InvalidVariableName(
                variable.to_string(),
            ));
        }

        let invalid = |reason| EnvironmentRuleValidationError::InvalidRule {
            variable: variable.to_string(),
            reason,
        };
        if !self.paths.is_empty() && !self.values.is_empty() {
            return Err(invalid("paths and literal values cannot be combined"));
        }
        if self
            .values
            .iter()
            .chain(&self.host_default)
            .any(|value| value.as_bytes().contains(&b'\0'))
        {
            return Err(invalid("literal values cannot contain NUL"));
        }
        let configured_is_empty = self.paths.is_empty() && self.values.is_empty();
        match self.operation {
            EnvironmentOperation::Replace => {
                if configured_is_empty {
                    return Err(invalid("replace requires paths or literal values"));
                }
                if self.inherit || !self.host_default.is_empty() {
                    return Err(invalid("replace cannot inherit host values"));
                }
            }
            EnvironmentOperation::Prepend | EnvironmentOperation::Append => {
                if configured_is_empty {
                    return Err(invalid(
                        "prepend and append require paths or literal values",
                    ));
                }
                if !self.inherit && !self.host_default.is_empty() {
                    return Err(invalid("host_default requires inherit = true"));
                }
            }
            EnvironmentOperation::Unset => {
                if !configured_is_empty || self.inherit || !self.host_default.is_empty() {
                    return Err(invalid(
                        "unset accepts no paths, values, or inheritance fields",
                    ));
                }
            }
            EnvironmentOperation::Default => {
                if self.inherit {
                    return Err(invalid("default does not use inherit"));
                }
                if configured_is_empty && self.host_default.is_empty() {
                    return Err(invalid(
                        "default requires paths, literal values, or host_default",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Invalid variable name or operation-specific environment rule shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentRuleValidationError {
    /// A variable name is empty or cannot be passed to `execve`.
    InvalidVariableName(String),
    /// An operation is combined with fields that have no defined meaning.
    InvalidRule {
        /// Variable whose rule is invalid.
        variable: String,
        /// Human-readable semantic failure.
        reason: &'static str,
    },
}

impl fmt::Display for EnvironmentRuleValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVariableName(name) => {
                write!(formatter, "invalid environment variable name '{name}'")
            }
            Self::InvalidRule { variable, reason } => {
                write!(
                    formatter,
                    "invalid environment rule for {variable}: {reason}"
                )
            }
        }
    }
}

impl Error for EnvironmentRuleValidationError {}

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
    /// A tool name cannot be selected safely through multi-call dispatch.
    InvalidToolName(String),
    /// The real launcher name is reserved for direct-mode invocation.
    ReservedToolName(String),
    /// `platform.min_kernel` is not a supported numeric version.
    InvalidMinimumKernel {
        /// Rejected version text.
        value: String,
        /// Specific syntax failure.
        reason: &'static str,
    },
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
            Self::InvalidToolName(name) => {
                write!(formatter, "invalid bundle tool name '{name}'")
            }
            Self::ReservedToolName(name) => {
                write!(
                    formatter,
                    "bundle tool name '{name}' is reserved by the launcher"
                )
            }
            Self::InvalidMinimumKernel { value, reason } => {
                write!(
                    formatter,
                    "invalid minimum kernel version '{value}': {reason}"
                )
            }
        }
    }
}

impl Error for BundleConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse(error) => Some(error),
            Self::UnsupportedFormat(_)
            | Self::NoTools
            | Self::InvalidToolName(_)
            | Self::ReservedToolName(_)
            | Self::InvalidMinimumKernel { .. } => None,
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
visibility = "public"

[tools.qemu-system-x86_64.environment.QEMU_AUDIO_DRV]
operation = "default"
values = ["none"]

[tools.qemu-img]
path = "root/usr/bin/qemu-img"
argv0 = "qemu-img"
visibility = "public"
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
    fn parses_and_serializes_aarch64_platform() {
        let source = COMPLETE_CONFIG.replace("arch = \"x86_64\"", "arch = \"aarch64\"");
        let config = BundleConfig::parse(&source).unwrap();

        assert_eq!(config.platform.arch, PlatformArch::Aarch64);
        assert_eq!(config.platform.arch.as_str(), "aarch64");
        assert_eq!(config.platform.arch.elf_machine(), 183);
        assert!(config.to_toml().unwrap().contains("arch = \"aarch64\""));
    }

    #[test]
    fn constructed_config_serializes_and_parses_without_semantic_changes() {
        let parsed = BundleConfig::parse(COMPLETE_CONFIG).unwrap();
        let constructed = BundleConfig::new_v1(
            parsed.payload_root.clone(),
            parsed.policy,
            parsed.platform.clone(),
            parsed.loader.clone(),
            parsed.environment.clone(),
            parsed.tools.clone(),
        )
        .unwrap();

        let serialized = constructed.to_toml().unwrap();
        let reparsed = BundleConfig::parse(&serialized).unwrap();

        assert!(serialized.ends_with('\n'));
        assert_eq!(reparsed, constructed);
        assert_eq!(reparsed.format(), BUNDLE_FORMAT_V1);
    }

    #[test]
    fn constructor_enforces_the_same_semantic_validation_as_parser() {
        let error = BundleConfig::new_v1(
            "root",
            HostPolicy::Strict,
            PlatformConfig {
                os: PlatformOs::Linux,
                arch: PlatformArch::X86_64,
                min_kernel: "invalid".to_string(),
            },
            LoaderConfig {
                kind: LoaderKind::Glibc,
                library_dirs: Vec::new(),
                inhibit_cache: true,
            },
            BTreeMap::new(),
            BTreeMap::from([(
                "demo".to_string(),
                ToolConfig {
                    path: "root/usr/bin/demo".to_string(),
                    argv0: "demo".to_string(),
                    visibility: ToolVisibility::Public,
                    environment: BTreeMap::new(),
                },
            )]),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BundleConfigError::InvalidMinimumKernel { .. }
        ));
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
visibility = "public"
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
    fn rejects_invalid_reserved_and_unreachable_tool_names() {
        for name in ["", ".", "..", "path/name", "bobr-bundle-launcher"] {
            let invalid =
                COMPLETE_CONFIG.replace("[tools.qemu-img]", &format!("[tools.\"{name}\"]"));
            let error = BundleConfig::parse(&invalid).unwrap_err();
            assert!(
                matches!(
                    error,
                    BundleConfigError::InvalidToolName(_) | BundleConfigError::ReservedToolName(_)
                ),
                "unexpected error for {name:?}: {error}"
            );
        }
    }

    #[test]
    fn rejects_invalid_minimum_kernel_version() {
        for value in ["", "linux-6.1", "6", "6.x", "6.1.2.3"] {
            let invalid = COMPLETE_CONFIG
                .replace("min_kernel = \"4.19\"", &format!("min_kernel = {value:?}"));
            assert!(matches!(
                BundleConfig::parse(&invalid),
                Err(BundleConfigError::InvalidMinimumKernel { .. })
            ));
        }
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
