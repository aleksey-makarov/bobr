//! HostBundle declaration, filesystem composition, and runtime-config lowering.

use crate::host_bundle_verify::{verify_startup_closure, verify_structure};
use crate::plain_tree_copy::{PlainTreeCopy, PlainTreeCopyFunction, PlainTreeCopyInput};
use crate::plain_tree_copy::{make_tree_read_only, verify_tree_read_only};
use crate::{BuildContext, BuilderError, BuilderInputs, InputSpec, TypedBuilder};
use bobr_bundle_launcher::{
    BundleConfig, BundleConfigError, EnvironmentOperation, EnvironmentRule,
    EnvironmentRuleValidationError, HostPolicy, LAUNCHER_BINARY_NAME, LoaderConfig, LoaderKind,
    PlatformArch, PlatformConfig, PlatformOs, ToolConfig, ToolResolutionError, ToolVisibility,
    validate_relative_path,
};
use bobr_core::BuildLogLevel;
use bobr_runtime::runtime::Runtime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

const PAYLOAD_ROOT: &str = "root";
const OVERRIDES_ROOT: &str = "overrides";
const WRAPPED_BIN: &str = "libexec/wrapped-bin";
const DEFAULT_MIN_KERNEL: &str = "4.19";
const OUTPUT_DIR_NAME: &str = "host-bundle";
const INPUT_LAUNCHER_PATH: &str = "usr/libexec/bobr-bundle-launcher";
const OUTPUT_LAUNCHER_PATH: &str = "libexec/bobr-bundle-launcher";

/// Materialized input contract for the HostBundle builder.
pub static HOST_BUNDLE_INPUT_SPEC: InputSpec = InputSpec {
    required_inputs: &["_root", "_launcher"],
    optional_inputs: &["_overrides"],
    allow_extra_inputs: false,
};

/// Builds a verified, read-only HostBundle directory object.
#[derive(Debug)]
pub struct HostBundleBuilder;

impl TypedBuilder for HostBundleBuilder {
    type Config = HostBundleConfig;

    fn tag(&self) -> &'static str {
        "HostBundle"
    }

    fn spec(&self) -> &'static InputSpec {
        &HOST_BUNDLE_INPUT_SPEC
    }

    fn impl_version(&self) -> &'static str {
        "1"
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError> {
        build_host_bundle(config, inputs, cx)
    }
}

fn build_host_bundle(
    config: HostBundleConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<PathBuf, BuilderError> {
    let has_overrides = inputs.optional("_overrides").is_some();
    let runtime_config = config
        .lower_runtime_config(has_overrides)
        .map_err(|error| BuilderError::InvalidRecipe(error.to_string()))?;
    let root = inputs.required("_root")?.clone();
    let launcher_tree = inputs.required("_launcher")?.clone();
    let output_root = cx.temp_dir.join(OUTPUT_DIR_NAME);

    let mut copies = vec![
        PlainTreeCopy::Tree {
            source: root,
            dest: PAYLOAD_ROOT.to_string(),
        },
        PlainTreeCopy::File {
            source: launcher_tree.join(INPUT_LAUNCHER_PATH),
            dest: OUTPUT_LAUNCHER_PATH.to_string(),
        },
    ];
    if let Some(overrides) = inputs.optional("_overrides") {
        copies.push(PlainTreeCopy::Tree {
            source: overrides.clone(),
            dest: OVERRIDES_ROOT.to_string(),
        });
    }

    cx.log_event(
        BuildLogLevel::Info,
        "compose",
        format!(
            "copying HostBundle payload and {} tool declaration(s)",
            runtime_config.tools.len()
        ),
    );
    cx.runtime()
        .run(
            &PlainTreeCopyFunction,
            PlainTreeCopyInput {
                output_root: output_root.clone(),
                copies,
            },
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?;

    materialize_facade(&output_root, &runtime_config)?;
    let structure =
        verify_structure(&output_root, &runtime_config).map_err(BuilderError::ExecutionFailed)?;
    verify_startup_closure(&output_root, &runtime_config, &structure)
        .map_err(BuilderError::ExecutionFailed)?;
    make_tree_read_only(&output_root).map_err(BuilderError::ExecutionFailed)?;
    verify_tree_read_only(&output_root).map_err(BuilderError::ExecutionFailed)?;
    Ok(output_root)
}

fn materialize_facade(
    output_root: &std::path::Path,
    runtime_config: &BundleConfig,
) -> Result<(), BuilderError> {
    let public_bin = output_root.join("bin");
    let wrapped_bin = output_root.join(WRAPPED_BIN);
    fs::create_dir(&public_bin).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create public bin directory '{}': {error}",
            public_bin.display()
        ))
    })?;
    fs::create_dir_all(&wrapped_bin).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create wrapped-bin directory '{}': {error}",
            wrapped_bin.display()
        ))
    })?;

    for (name, tool) in &runtime_config.tools {
        let wrapped = wrapped_bin.join(name);
        symlink(format!("../{LAUNCHER_BINARY_NAME}"), &wrapped).map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to create internal wrapper '{}': {error}",
                wrapped.display()
            ))
        })?;
        if tool.visibility == ToolVisibility::Public {
            let public = public_bin.join(name);
            symlink(format!("../libexec/{LAUNCHER_BINARY_NAME}"), &public).map_err(|error| {
                BuilderError::ExecutionFailed(format!(
                    "failed to create public wrapper '{}': {error}",
                    public.display()
                ))
            })?;
        }
    }

    let config_path = output_root.join("bundle.toml");
    let config_contents = runtime_config.to_toml().map_err(|error| {
        BuilderError::ExecutionFailed(format!("failed to serialize bundle.toml: {error}"))
    })?;
    fs::write(&config_path, config_contents).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to write '{}': {error}",
            config_path.display()
        ))
    })
}

/// User-facing build-time declaration of one HostBundle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostBundleConfig {
    /// Host-integration policy recorded in the runtime configuration.
    #[serde(default = "default_policy")]
    pub policy: HostPolicy,
    /// Minimum Linux kernel version accepted by the launcher.
    #[serde(default = "default_min_kernel")]
    pub min_kernel: String,
    /// Ordered library directories relative to the payload root.
    pub library_dirs: Vec<String>,
    /// Commands exposed through the bundle's top-level `bin/`.
    pub public_tools: BTreeMap<String, HostBundleToolConfig>,
    /// Helpers exposed only through `libexec/wrapped-bin/`.
    #[serde(default)]
    pub internal_tools: BTreeMap<String, HostBundleToolConfig>,
    /// Environment rules shared by every configured tool.
    #[serde(default)]
    pub environment: BTreeMap<String, HostBundleEnvironmentRule>,
}

/// Build-time declaration of one public command or internal helper.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostBundleToolConfig {
    /// Executable path relative to the payload root.
    pub path: String,
    /// Optional logical `argv[0]`; the tool name is used when omitted.
    #[serde(default)]
    pub argv0: Option<String>,
    /// Environment rules applied after the common rules for this tool.
    #[serde(default)]
    pub environment: BTreeMap<String, HostBundleEnvironmentRule>,
}

/// One environment operation in a build-time HostBundle declaration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostBundleEnvironmentRule {
    /// How configured entries and an inherited value are combined.
    pub operation: EnvironmentOperation,
    /// Ordered paths whose source namespace is explicit.
    #[serde(default)]
    pub paths: Vec<HostBundlePath>,
    /// Ordered literal entries that do not undergo path resolution.
    #[serde(default)]
    pub values: Vec<String>,
    /// Whether the inherited host value participates in this operation.
    #[serde(default)]
    pub inherit: bool,
    /// Literal fallback entries used only when a host variable is absent.
    #[serde(default)]
    pub host_default: Vec<String>,
}

/// A path selected from one of the HostBundle's copied input namespaces.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum HostBundlePath {
    /// A path relative to the `_root` payload.
    Payload {
        /// Relative path inside the payload.
        path: String,
    },
    /// A path relative to the optional `_overrides` tree.
    Overrides {
        /// Relative path inside the overrides tree.
        path: String,
    },
}

/// Invalid HostBundle declaration encountered before filesystem composition.
#[derive(Debug)]
pub enum HostBundleConfigError {
    /// An application bundle must expose at least one public command.
    NoPublicTools,
    /// A name appears in both the public and internal tool maps.
    DuplicateTool(String),
    /// The mandatory wrapped-bin prefix makes user PATH rules ambiguous.
    PathRuleForbidden {
        /// Configuration scope that declared `PATH`.
        scope: String,
    },
    /// A declaration path is lexically unsafe.
    InvalidPath {
        /// Underlying path validation failure.
        source: ToolResolutionError,
    },
    /// An overrides path was declared without an `_overrides` input.
    MissingOverridesInput {
        /// Configuration field containing the path.
        field: String,
        /// Relative overrides path requested by the declaration.
        path: String,
    },
    /// Explicit `argv0` is empty or contains a NUL.
    InvalidArgv0 {
        /// Tool whose logical process name is invalid.
        tool: String,
    },
    /// An environment operation has an invalid field combination.
    InvalidEnvironmentRule {
        /// Configuration scope containing the rule.
        scope: String,
        /// Shared runtime-rule validation failure.
        source: EnvironmentRuleValidationError,
    },
    /// Lowering produced a runtime config rejected by the launcher model.
    RuntimeConfig(BundleConfigError),
}

impl fmt::Display for HostBundleConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPublicTools => {
                formatter.write_str("HostBundle must define at least one public tool")
            }
            Self::DuplicateTool(tool) => write!(
                formatter,
                "HostBundle tool '{tool}' is declared as both public and internal"
            ),
            Self::PathRuleForbidden { scope } => write!(
                formatter,
                "{scope}.PATH is reserved for the mandatory wrapped-bin policy"
            ),
            Self::InvalidPath { source } => source.fmt(formatter),
            Self::MissingOverridesInput { field, path } => write!(
                formatter,
                "{field} references overrides path '{path}' but input '_overrides' is absent"
            ),
            Self::InvalidArgv0 { tool } => {
                write!(formatter, "HostBundle tool '{tool}' has an invalid argv0")
            }
            Self::InvalidEnvironmentRule { scope, source } => {
                write!(formatter, "invalid rule in {scope}: {source}")
            }
            Self::RuntimeConfig(error) => {
                write!(formatter, "invalid generated bundle.toml: {error}")
            }
        }
    }
}

impl Error for HostBundleConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPath { source } => Some(source),
            Self::InvalidEnvironmentRule { source, .. } => Some(source),
            Self::RuntimeConfig(error) => Some(error),
            Self::NoPublicTools
            | Self::DuplicateTool(_)
            | Self::PathRuleForbidden { .. }
            | Self::MissingOverridesInput { .. }
            | Self::InvalidArgv0 { .. } => None,
        }
    }
}

impl HostBundleConfig {
    /// Lowers this build-time declaration into the canonical runtime model.
    ///
    /// `has_overrides` records whether the optional `_overrides` input is
    /// present. No input filesystem is read at this stage.
    pub fn lower_runtime_config(
        &self,
        has_overrides: bool,
    ) -> Result<BundleConfig, HostBundleConfigError> {
        if self.public_tools.is_empty() {
            return Err(HostBundleConfigError::NoPublicTools);
        }
        for name in self.public_tools.keys() {
            if self.internal_tools.contains_key(name) {
                return Err(HostBundleConfigError::DuplicateTool(name.clone()));
            }
        }
        if self.environment.contains_key("PATH") {
            return Err(HostBundleConfigError::PathRuleForbidden {
                scope: "environment".to_string(),
            });
        }
        let library_dirs = self
            .library_dirs
            .iter()
            .enumerate()
            .map(|(index, path)| {
                validate_relative_path(&format!("library_dirs[{index}]"), path)
                    .map_err(|source| HostBundleConfigError::InvalidPath { source })?;
                Ok(format!("{PAYLOAD_ROOT}/{path}"))
            })
            .collect::<Result<Vec<_>, HostBundleConfigError>>()?;

        let mut environment = lower_environment("environment", &self.environment, has_overrides)?;
        environment.insert(
            "PATH".to_string(),
            EnvironmentRule {
                operation: EnvironmentOperation::Prepend,
                paths: vec![WRAPPED_BIN.to_string()],
                values: Vec::new(),
                inherit: true,
                host_default: Vec::new(),
            },
        );

        let mut tools = BTreeMap::new();
        lower_tools(
            "public_tools",
            &self.public_tools,
            ToolVisibility::Public,
            has_overrides,
            &mut tools,
        )?;
        lower_tools(
            "internal_tools",
            &self.internal_tools,
            ToolVisibility::Internal,
            has_overrides,
            &mut tools,
        )?;

        BundleConfig::new_v1(
            PAYLOAD_ROOT,
            self.policy,
            PlatformConfig {
                os: PlatformOs::Linux,
                arch: PlatformArch::X86_64,
                min_kernel: self.min_kernel.clone(),
            },
            LoaderConfig {
                kind: LoaderKind::Glibc,
                library_dirs,
                inhibit_cache: true,
            },
            environment,
            tools,
        )
        .map_err(HostBundleConfigError::RuntimeConfig)
    }
}

fn lower_tools(
    scope: &str,
    declarations: &BTreeMap<String, HostBundleToolConfig>,
    visibility: ToolVisibility,
    has_overrides: bool,
    output: &mut BTreeMap<String, ToolConfig>,
) -> Result<(), HostBundleConfigError> {
    for (name, tool) in declarations {
        let path_field = format!("{scope}.{name}.path");
        validate_relative_path(&path_field, &tool.path)
            .map_err(|source| HostBundleConfigError::InvalidPath { source })?;
        let argv0 = tool.argv0.clone().unwrap_or_else(|| name.clone());
        if argv0.is_empty() || argv0.as_bytes().contains(&b'\0') {
            return Err(HostBundleConfigError::InvalidArgv0 { tool: name.clone() });
        }
        let environment_scope = format!("{scope}.{name}.environment");
        if tool.environment.contains_key("PATH") {
            return Err(HostBundleConfigError::PathRuleForbidden {
                scope: environment_scope,
            });
        }
        let environment = lower_environment(&environment_scope, &tool.environment, has_overrides)?;
        output.insert(
            name.clone(),
            ToolConfig {
                path: format!("{PAYLOAD_ROOT}/{}", tool.path),
                argv0,
                visibility,
                environment,
            },
        );
    }
    Ok(())
}

fn lower_environment(
    scope: &str,
    declarations: &BTreeMap<String, HostBundleEnvironmentRule>,
    has_overrides: bool,
) -> Result<BTreeMap<String, EnvironmentRule>, HostBundleConfigError> {
    declarations
        .iter()
        .map(|(variable, declaration)| {
            let paths = declaration
                .paths
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    lower_path(
                        &format!("{scope}.{variable}.paths[{index}]"),
                        path,
                        has_overrides,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            let rule = EnvironmentRule {
                operation: declaration.operation,
                paths,
                values: declaration.values.clone(),
                inherit: declaration.inherit,
                host_default: declaration.host_default.clone(),
            };
            rule.validate(variable).map_err(|source| {
                HostBundleConfigError::InvalidEnvironmentRule {
                    scope: format!("{scope}.{variable}"),
                    source,
                }
            })?;
            Ok((variable.clone(), rule))
        })
        .collect()
}

fn lower_path(
    field: &str,
    declaration: &HostBundlePath,
    has_overrides: bool,
) -> Result<String, HostBundleConfigError> {
    let (root, path) = match declaration {
        HostBundlePath::Payload { path } => (PAYLOAD_ROOT, path),
        HostBundlePath::Overrides { path } => {
            if !has_overrides {
                return Err(HostBundleConfigError::MissingOverridesInput {
                    field: field.to_string(),
                    path: path.clone(),
                });
            }
            (OVERRIDES_ROOT, path)
        }
    };
    validate_relative_path(field, path)
        .map_err(|source| HostBundleConfigError::InvalidPath { source })?;
    Ok(format!("{root}/{path}"))
}

fn default_policy() -> HostPolicy {
    HostPolicy::Strict
}

fn default_min_kernel() -> String {
    DEFAULT_MIN_KERNEL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::store_fs_tree;
    use bobr_bundle_launcher::{BUNDLE_FORMAT_V1, ToolVisibility};
    use serde_json::json;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn config(value: serde_json::Value) -> HostBundleConfig {
        serde_json::from_value(value).unwrap()
    }

    fn minimal_config() -> HostBundleConfig {
        config(json!({
            "library_dirs": ["usr/lib64", "usr/lib"],
            "public_tools": {
                "mc": {
                    "path": "usr/bin/mc"
                }
            }
        }))
    }

    fn write_static_elf(path: &std::path::Path) {
        let mut bytes = vec![0_u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn input_contract_materializes_only_declared_trees() {
        assert_eq!(
            HOST_BUNDLE_INPUT_SPEC.required_inputs,
            &["_root", "_launcher"]
        );
        assert_eq!(HOST_BUNDLE_INPUT_SPEC.optional_inputs, &["_overrides"]);
        assert!(!HOST_BUNDLE_INPUT_SPEC.allow_extra_inputs);
        HOST_BUNDLE_INPUT_SPEC.validate().unwrap();
    }

    #[test]
    fn serde_defaults_produce_a_canonical_builder_config() {
        let config = minimal_config();
        let canonical = serde_json::to_value(&config).unwrap();

        assert_eq!(config.policy, HostPolicy::Strict);
        assert_eq!(config.min_kernel, DEFAULT_MIN_KERNEL);
        assert_eq!(config.library_dirs, ["usr/lib64", "usr/lib"]);
        assert!(config.internal_tools.is_empty());
        assert!(config.environment.is_empty());
        assert!(config.public_tools["mc"].argv0.is_none());
        assert_eq!(canonical["policy"], "strict");
        assert_eq!(canonical["min_kernel"], DEFAULT_MIN_KERNEL);
        assert_eq!(canonical["library_dirs"], json!(["usr/lib64", "usr/lib"]));
        assert_eq!(canonical["internal_tools"], json!({}));
        assert_eq!(canonical["environment"], json!({}));
        assert_eq!(canonical["public_tools"]["mc"]["argv0"], json!(null));
    }

    #[test]
    fn builder_config_rejects_unknown_fields() {
        let error = serde_json::from_value::<HostBundleConfig>(json!({
            "library_dirs": [],
            "public_tools": {},
            "format": "user-selected"
        }))
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("format"));
    }

    #[test]
    fn lowers_tools_paths_environment_and_builder_owned_fields() {
        let config = config(json!({
            "policy": "integrated",
            "min_kernel": "5.10",
            "library_dirs": ["usr/lib/x86_64-linux-gnu", "lib64"],
            "public_tools": {
                "mc": {
                    "path": "usr/bin/mc"
                }
            },
            "internal_tools": {
                "sh": {
                    "path": "usr/bin/bash",
                    "argv0": "sh",
                    "environment": {
                        "SHELL_MODE": {
                            "operation": "replace",
                            "values": ["internal"]
                        }
                    }
                }
            },
            "environment": {
                "TERMINFO_DIRS": {
                    "operation": "replace",
                    "paths": [
                        {
                            "source": "payload",
                            "path": "usr/share/terminfo"
                        }
                    ]
                },
                "VK_DRIVER_FILES": {
                    "operation": "replace",
                    "paths": [
                        {
                            "source": "overrides",
                            "path": "vulkan/icd.json"
                        }
                    ]
                }
            }
        }));

        let runtime = config.lower_runtime_config(true).unwrap();

        assert_eq!(runtime.format(), BUNDLE_FORMAT_V1);
        assert_eq!(runtime.payload_root, "root");
        assert_eq!(runtime.policy, HostPolicy::Integrated);
        assert_eq!(runtime.platform.min_kernel, "5.10");
        assert_eq!(
            runtime.loader.library_dirs,
            ["root/usr/lib/x86_64-linux-gnu", "root/lib64"]
        );
        assert!(runtime.loader.inhibit_cache);
        assert_eq!(runtime.tools["mc"].path, "root/usr/bin/mc");
        assert_eq!(runtime.tools["mc"].argv0, "mc");
        assert_eq!(runtime.tools["mc"].visibility, ToolVisibility::Public);
        assert_eq!(runtime.tools["sh"].path, "root/usr/bin/bash");
        assert_eq!(runtime.tools["sh"].argv0, "sh");
        assert_eq!(runtime.tools["sh"].visibility, ToolVisibility::Internal);
        assert_eq!(
            runtime.environment["TERMINFO_DIRS"].paths,
            ["root/usr/share/terminfo"]
        );
        assert_eq!(
            runtime.environment["VK_DRIVER_FILES"].paths,
            ["overrides/vulkan/icd.json"]
        );
        assert_eq!(
            runtime.environment["PATH"],
            EnvironmentRule {
                operation: EnvironmentOperation::Prepend,
                paths: vec!["libexec/wrapped-bin".to_string()],
                values: Vec::new(),
                inherit: true,
                host_default: Vec::new(),
            }
        );
    }

    #[test]
    fn lowered_runtime_config_round_trips_through_toml() {
        let runtime = minimal_config().lower_runtime_config(false).unwrap();

        let toml = runtime.to_toml().unwrap();
        let reparsed = BundleConfig::parse(&toml).unwrap();

        assert_eq!(reparsed, runtime);
        assert!(!toml.contains("public_tools"));
        assert!(!toml.contains("internal_tools"));
        assert!(toml.contains("format = \"bobr-host-bundle-v1\""));
    }

    #[test]
    fn rejects_empty_public_set_and_public_internal_collision() {
        let no_public = config(json!({
            "library_dirs": [],
            "public_tools": {},
            "internal_tools": {
                "helper": { "path": "usr/bin/helper" }
            }
        }));
        assert!(matches!(
            no_public.lower_runtime_config(false),
            Err(HostBundleConfigError::NoPublicTools)
        ));

        let collision = config(json!({
            "library_dirs": [],
            "public_tools": {
                "tool": { "path": "usr/bin/tool" }
            },
            "internal_tools": {
                "tool": { "path": "usr/libexec/tool" }
            }
        }));
        assert!(matches!(
            collision.lower_runtime_config(false),
            Err(HostBundleConfigError::DuplicateTool(tool)) if tool == "tool"
        ));
    }

    #[test]
    fn rejects_common_and_per_tool_path_rules() {
        let common = config(json!({
            "library_dirs": [],
            "public_tools": {
                "tool": { "path": "usr/bin/tool" }
            },
            "environment": {
                "PATH": {
                    "operation": "replace",
                    "values": ["/host/bin"]
                }
            }
        }));
        assert!(matches!(
            common.lower_runtime_config(false),
            Err(HostBundleConfigError::PathRuleForbidden { .. })
        ));

        let per_tool = config(json!({
            "library_dirs": [],
            "public_tools": {
                "tool": {
                    "path": "usr/bin/tool",
                    "environment": {
                        "PATH": {
                            "operation": "unset"
                        }
                    }
                }
            }
        }));
        assert!(matches!(
            per_tool.lower_runtime_config(false),
            Err(HostBundleConfigError::PathRuleForbidden { .. })
        ));
    }

    #[test]
    fn rejects_missing_overrides_unsafe_paths_and_invalid_rule_shapes() {
        let overrides = config(json!({
            "library_dirs": [],
            "public_tools": {
                "tool": { "path": "usr/bin/tool" }
            },
            "environment": {
                "DATA": {
                    "operation": "replace",
                    "paths": [
                        { "source": "overrides", "path": "data/file" }
                    ]
                }
            }
        }));
        assert!(matches!(
            overrides.lower_runtime_config(false),
            Err(HostBundleConfigError::MissingOverridesInput { .. })
        ));

        let unsafe_tool = config(json!({
            "library_dirs": [],
            "public_tools": {
                "tool": { "path": "../usr/bin/tool" }
            }
        }));
        assert!(matches!(
            unsafe_tool.lower_runtime_config(false),
            Err(HostBundleConfigError::InvalidPath { .. })
        ));

        let mixed = config(json!({
            "library_dirs": [],
            "public_tools": {
                "tool": { "path": "usr/bin/tool" }
            },
            "environment": {
                "DATA": {
                    "operation": "replace",
                    "paths": [
                        { "source": "payload", "path": "usr/share" }
                    ],
                    "values": ["literal"]
                }
            }
        }));
        assert!(matches!(
            mixed.lower_runtime_config(false),
            Err(HostBundleConfigError::InvalidEnvironmentRule { .. })
        ));
    }

    #[test]
    fn composes_independent_payload_launcher_config_and_wrappers() {
        let temp = tempdir().unwrap();
        let payload = temp.path().join("payload");
        let launcher = temp.path().join("launcher");
        let overrides = temp.path().join("overrides");
        fs::create_dir_all(payload.join("usr/bin")).unwrap();
        fs::create_dir_all(payload.join("usr/lib")).unwrap();
        fs::create_dir_all(launcher.join("usr/libexec")).unwrap();
        fs::create_dir(&overrides).unwrap();
        write_static_elf(&payload.join("usr/bin/demo"));
        write_static_elf(&launcher.join(INPUT_LAUNCHER_PATH));
        fs::write(overrides.join("registry"), b"override").unwrap();

        let mut slots = BTreeMap::new();
        slots.insert("_root".to_string(), payload.clone());
        slots.insert("_launcher".to_string(), launcher);
        slots.insert("_overrides".to_string(), overrides);
        let build_temp = temp.path().join("build");
        fs::create_dir(&build_temp).unwrap();
        let mut cx = BuildContext::with_noop_logger(build_temp, store_fs_tree(temp.path()));
        let config = config(json!({
            "library_dirs": ["usr/lib"],
            "public_tools": {
                "demo": { "path": "usr/bin/demo" }
            },
            "internal_tools": {
                "helper": { "path": "usr/bin/demo", "argv0": "helper" }
            }
        }));

        let builder = crate::BUILDERS
            .iter()
            .copied()
            .find(|builder| builder.tag() == "HostBundle")
            .unwrap();
        let plan = builder.plan(serde_json::to_value(config).unwrap()).unwrap();
        let output = plan.build(BuilderInputs::new(slots), &mut cx).unwrap();

        assert_eq!(
            fs::read_link(output.join("bin/demo")).unwrap(),
            PathBuf::from("../libexec/bobr-bundle-launcher")
        );
        assert_eq!(
            fs::read_link(output.join("libexec/wrapped-bin/demo")).unwrap(),
            PathBuf::from("../bobr-bundle-launcher")
        );
        assert_eq!(
            fs::read_link(output.join("libexec/wrapped-bin/helper")).unwrap(),
            PathBuf::from("../bobr-bundle-launcher")
        );
        assert!(!output.join("bin/helper").exists());
        assert_eq!(
            fs::read(output.join("overrides/registry")).unwrap(),
            b"override"
        );
        assert_ne!(
            fs::metadata(payload.join("usr/bin/demo")).unwrap().ino(),
            fs::metadata(output.join("root/usr/bin/demo"))
                .unwrap()
                .ino()
        );

        let runtime =
            BundleConfig::parse(&fs::read_to_string(output.join("bundle.toml")).unwrap()).unwrap();
        assert_eq!(runtime.loader.library_dirs, ["root/usr/lib"]);
        assert_eq!(runtime.tools["demo"].visibility, ToolVisibility::Public);
        assert_eq!(runtime.tools["helper"].visibility, ToolVisibility::Internal);
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o555
        );
        verify_tree_read_only(&output).unwrap();
    }

    #[test]
    fn registered_builder_rejects_an_invalid_launcher_during_execution() {
        let temp = tempdir().unwrap();
        let payload = temp.path().join("payload");
        let launcher = temp.path().join("launcher");
        fs::create_dir_all(payload.join("usr/bin")).unwrap();
        fs::create_dir_all(payload.join("usr/lib")).unwrap();
        fs::create_dir_all(launcher.join("usr/libexec")).unwrap();
        write_static_elf(&payload.join("usr/bin/demo"));
        fs::write(launcher.join(INPUT_LAUNCHER_PATH), b"not an ELF").unwrap();
        fs::set_permissions(
            launcher.join(INPUT_LAUNCHER_PATH),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let build_temp = temp.path().join("build");
        fs::create_dir(&build_temp).unwrap();
        let mut cx = BuildContext::with_noop_logger(build_temp, store_fs_tree(temp.path()));
        let inputs = BuilderInputs::new(BTreeMap::from([
            ("_root".to_string(), payload),
            ("_launcher".to_string(), launcher),
        ]));
        let builder = crate::BUILDERS
            .iter()
            .copied()
            .find(|builder| builder.tag() == "HostBundle")
            .unwrap();
        let plan = builder
            .plan(json!({
                "library_dirs": ["usr/lib"],
                "public_tools": {
                    "demo": { "path": "usr/bin/demo" }
                }
            }))
            .unwrap();

        let error = plan.build(inputs, &mut cx).unwrap_err();

        assert!(error.to_string().contains("invalid HostBundle launcher"));
    }
}
