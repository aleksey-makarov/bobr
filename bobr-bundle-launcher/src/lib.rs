//! Runtime support for launching programs packaged in a bobr HostBundle.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

mod invocation;
mod location;

pub use invocation::{Invocation, InvocationError, parse_invocation};
pub use location::{
    BUNDLE_CONFIG_NAME, BUNDLE_LIBEXEC_DIR, BundleLocation, BundleLocationError,
    LAUNCHER_BINARY_NAME, locate_bundle_from_launcher, locate_current_bundle,
};
