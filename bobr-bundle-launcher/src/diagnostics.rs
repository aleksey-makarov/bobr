//! Human- and machine-readable launch diagnostics.

use crate::{
    BundleConfig, BundleLocation, ElfLinkage, EnvironmentOperation, EnvironmentOrigin,
    ExecutableFormat, HostPlatformCheck, HostPolicy, PreparedToolLaunch, ProcessEnvironment,
    ResolvedTool, ToolVisibility,
};
use serde::Serialize;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

/// Complete diagnostic description of a prepared launch.
#[derive(Debug, Serialize)]
pub struct DiagnosticReport {
    bundle_root: String,
    payload_root: String,
    policy: &'static str,
    platform: PlatformDiagnostic,
    tool: ToolDiagnostic,
    executable: ExecutableDiagnostic,
    environment: Vec<EnvironmentDiagnostic>,
}

#[derive(Debug, Serialize)]
struct PlatformDiagnostic {
    required_os: &'static str,
    required_arch: &'static str,
    minimum_kernel: String,
    host_kernel: String,
    os_compatible: bool,
    arch_compatible: bool,
    kernel_compatible: bool,
    compatible: bool,
}

#[derive(Debug, Serialize)]
struct ToolDiagnostic {
    name: String,
    visibility: &'static str,
    target: String,
}

#[derive(Debug, Serialize)]
struct ExecutableDiagnostic {
    format: &'static str,
    linkage: &'static str,
    position_independent: Option<bool>,
    interpreter: Option<String>,
    loader: Option<String>,
    library_path: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EnvironmentDiagnostic {
    name: String,
    value: String,
    origin: String,
}

impl DiagnosticReport {
    /// Constructs diagnostics from a completely validated launch plan.
    pub fn new(
        location: &BundleLocation,
        bundle: &BundleConfig,
        tool: &ResolvedTool,
        environment: &ProcessEnvironment,
        launch: &PreparedToolLaunch,
        host: &HostPlatformCheck,
    ) -> Self {
        let (format, linkage, position_independent, interpreter) = match launch.format() {
            ExecutableFormat::Elf(elf) => (
                "elf",
                match elf.linkage() {
                    ElfLinkage::Static => "static",
                    ElfLinkage::Dynamic { .. } => "dynamic",
                },
                Some(elf.is_position_independent()),
                match elf.linkage() {
                    ElfLinkage::Dynamic { interpreter } => Some(escape_os(interpreter.as_os_str())),
                    ElfLinkage::Static => None,
                },
            ),
            ExecutableFormat::Script(shebang) => (
                "script",
                "script",
                None,
                Some(escape_os(shebang.interpreter().as_os_str())),
            ),
        };
        let dynamic = launch.process().dynamic();
        let executable = ExecutableDiagnostic {
            format,
            linkage,
            position_independent,
            interpreter,
            loader: launch
                .process()
                .loader()
                .map(|path| escape_os(path.as_os_str())),
            library_path: dynamic
                .map(|plan| {
                    plan.library_dirs()
                        .iter()
                        .map(|path| escape_os(path.as_os_str()))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let environment = environment
            .iter()
            .map(|(name, value)| EnvironmentDiagnostic {
                name: escape_os(name),
                value: escape_os(value),
                origin: environment
                    .trace(name)
                    .map(|trace| {
                        trace
                            .iter()
                            .copied()
                            .map(origin_name)
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    })
                    .unwrap_or_else(|| "unknown".to_string()),
            })
            .collect();

        Self {
            bundle_root: escape_os(location.root().as_os_str()),
            payload_root: escape_os(tool.payload_root().as_os_str()),
            policy: match bundle.policy {
                HostPolicy::Strict => "strict",
                HostPolicy::Integrated => "integrated",
            },
            platform: PlatformDiagnostic {
                required_os: "linux",
                required_arch: "x86_64",
                minimum_kernel: bundle.platform.min_kernel.clone(),
                host_kernel: host.kernel_release().to_string(),
                os_compatible: host.os_compatible(),
                arch_compatible: host.arch_compatible(),
                kernel_compatible: host.kernel_compatible(),
                compatible: host.is_compatible(),
            },
            tool: ToolDiagnostic {
                name: tool.name().to_string(),
                visibility: match tool.config().visibility {
                    ToolVisibility::Public => "public",
                    ToolVisibility::Internal => "internal",
                },
                target: escape_os(tool.target().as_os_str()),
            },
            executable,
            environment,
        }
    }

    /// Serializes deterministic pretty-printed JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("DiagnosticReport is always serializable")
    }

    /// Renders stable line-oriented diagnostics for people and shell tests.
    pub fn to_human(&self) -> String {
        let mut lines = vec![
            format!("bundle_root={}", self.bundle_root),
            format!("payload_root={}", self.payload_root),
            format!("policy={}", self.policy),
            format!("platform.required_os={}", self.platform.required_os),
            format!("platform.required_arch={}", self.platform.required_arch),
            format!("platform.minimum_kernel={}", self.platform.minimum_kernel),
            format!("platform.host_kernel={}", self.platform.host_kernel),
            format!("platform.compatible={}", self.platform.compatible),
            format!("tool={}", self.tool.name),
            format!("visibility={}", self.tool.visibility),
            format!("target={}", self.tool.target),
            format!("format={}", self.executable.format),
            format!("linkage={}", self.executable.linkage),
        ];
        if let Some(position_independent) = self.executable.position_independent {
            lines.push(format!("position_independent={position_independent}"));
        }
        if let Some(interpreter) = &self.executable.interpreter {
            lines.push(format!("interpreter={interpreter}"));
        }
        if let Some(loader) = &self.executable.loader {
            lines.push(format!("loader={loader}"));
        }
        lines.push(format!(
            "library_path={}",
            self.executable.library_path.join(":")
        ));
        for entry in &self.environment {
            lines.push(format!(
                "environment.{}={} [{}]",
                entry.name, entry.value, entry.origin
            ));
        }
        lines.join("\n")
    }
}

fn origin_name(origin: EnvironmentOrigin) -> String {
    match origin {
        EnvironmentOrigin::Host => "host".to_string(),
        EnvironmentOrigin::Common(operation) => {
            format!("common:{}", operation_name(operation))
        }
        EnvironmentOrigin::Tool(operation) => format!("tool:{}", operation_name(operation)),
    }
}

fn operation_name(operation: EnvironmentOperation) -> &'static str {
    match operation {
        EnvironmentOperation::Replace => "replace",
        EnvironmentOperation::Prepend => "prepend",
        EnvironmentOperation::Append => "append",
        EnvironmentOperation::Unset => "unset",
        EnvironmentOperation::Default => "default",
    }
}

fn escape_os(value: &OsStr) -> String {
    match value.to_str() {
        Some(value) => value.to_string(),
        None => {
            let mut escaped = String::new();
            for byte in value.as_bytes() {
                if byte.is_ascii_graphic() || *byte == b' ' {
                    escaped.push(char::from(*byte));
                } else {
                    escaped.push_str(&format!("\\x{byte:02x}"));
                }
            }
            escaped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn escapes_non_utf8_diagnostic_values_deterministically() {
        let value = OsString::from_vec(vec![b'a', 0xff, b'\n']);
        assert_eq!(escape_os(&value), "a\\xff\\x0a");
    }
}
