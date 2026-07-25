//! Runtime support for launching programs packaged in a bobr HostBundle.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod config;
mod dispatch;
mod dynamic;
mod elf;
mod environment;
mod invocation;
mod launch;
mod location;
mod script;
mod tool;

pub use config::{
    BUNDLE_FORMAT_V1, BundleConfig, BundleConfigError, EnvironmentOperation, EnvironmentRule,
    HostPolicy, LoaderConfig, LoaderKind, PlatformArch, PlatformConfig, PlatformOs, ToolConfig,
    read_bundle_config,
};
pub use dispatch::{DispatchError, PreparedToolLaunch, ProcessLaunchPlan, prepare_tool_launch};
pub use dynamic::{
    DynamicLaunchError, DynamicLaunchPlan, prepare_dynamic_launch, prepare_dynamic_program,
};
pub use elf::{ElfError, ElfExecutable, ElfLinkage, inspect_elf};
pub use environment::{EnvironmentError, ProcessEnvironment, build_environment};
pub use invocation::{Invocation, InvocationError, parse_invocation};
pub use launch::{exec_dynamic, exec_prepared, exec_static};
pub use location::{
    BUNDLE_CONFIG_NAME, BUNDLE_LIBEXEC_DIR, BundleLocation, BundleLocationError,
    LAUNCHER_BINARY_NAME, locate_bundle_from_launcher, locate_current_bundle,
};
pub use script::{ExecutableFormat, ExecutableInspectionError, Shebang, inspect_executable};
pub use tool::{ResolvedTool, ToolResolutionError, resolve_tool};
