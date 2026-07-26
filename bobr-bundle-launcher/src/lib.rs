//! Runtime support for launching programs packaged in a bobr HostBundle.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod config;
mod diagnostics;
mod dispatch;
mod dynamic;
mod elf;
mod environment;
mod invocation;
mod launch;
mod location;
mod platform;
mod script;
mod tool;

pub use config::{
    BUNDLE_FORMAT_V1, BundleConfig, BundleConfigError, EnvironmentOperation, EnvironmentRule,
    HostPolicy, LoaderConfig, LoaderKind, PlatformArch, PlatformConfig, PlatformOs, ToolConfig,
    ToolVisibility, read_bundle_config,
};
pub use diagnostics::DiagnosticReport;
pub use dispatch::{DispatchError, PreparedToolLaunch, ProcessLaunchPlan, prepare_tool_launch};
pub use dynamic::{
    DynamicLaunchError, DynamicLaunchPlan, prepare_dynamic_launch, prepare_dynamic_program,
};
pub use elf::{ElfError, ElfExecutable, ElfLinkage, inspect_elf, inspect_elf_for_arch};
pub use environment::{EnvironmentError, EnvironmentOrigin, ProcessEnvironment, build_environment};
pub use invocation::{DiagnosticOutput, Invocation, InvocationError, parse_invocation};
pub use launch::exec_prepared;
pub use location::{
    BUNDLE_CONFIG_NAME, BUNDLE_LIBEXEC_DIR, BundleLocation, BundleLocationError,
    LAUNCHER_BINARY_NAME, locate_bundle_from_launcher, locate_current_bundle,
};
pub use platform::{HostPlatformCheck, HostPlatformError, check_host_platform};
pub use script::{
    ExecutableFormat, ExecutableInspectionError, Shebang, inspect_executable,
    inspect_executable_for_arch,
};
pub use tool::{ResolvedTool, ToolResolutionError, resolve_tool};
