//! Host platform checks required before entering a HostBundle payload.

use crate::{PlatformArch, PlatformConfig, PlatformOs};
use std::error::Error;
use std::ffi::CStr;
use std::fmt;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct KernelVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl KernelVersion {
    pub(crate) fn parse_required(value: &str) -> Result<Self, &'static str> {
        let parts = value.split('.').collect::<Vec<_>>();
        if !(2..=3).contains(&parts.len()) {
            return Err("expected MAJOR.MINOR or MAJOR.MINOR.PATCH");
        }
        let mut numbers = [0_u64; 3];
        for (index, part) in parts.iter().enumerate() {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("version components must contain decimal digits");
            }
            numbers[index] = part.parse().map_err(|_| "version component is too large")?;
        }
        Ok(Self {
            major: numbers[0],
            minor: numbers[1],
            patch: numbers[2],
        })
    }

    fn parse_release(value: &str) -> Result<Self, HostPlatformError> {
        let numeric = value
            .split_once('-')
            .map_or(value, |(numeric, _suffix)| numeric);
        let mut parts = numeric.split('.');
        let component = |part: Option<&str>| {
            part.and_then(|value| {
                let digits = value
                    .bytes()
                    .take_while(u8::is_ascii_digit)
                    .collect::<Vec<_>>();
                (!digits.is_empty())
                    .then(|| std::str::from_utf8(&digits).ok()?.parse::<u64>().ok())
                    .flatten()
            })
        };
        let Some(major) = component(parts.next()) else {
            return Err(HostPlatformError::InvalidKernelRelease(value.to_string()));
        };
        let Some(minor) = component(parts.next()) else {
            return Err(HostPlatformError::InvalidKernelRelease(value.to_string()));
        };
        let patch = component(parts.next()).unwrap_or(0);
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for KernelVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Result of comparing the current kernel and target with bundle requirements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPlatformCheck {
    kernel_release: String,
    kernel_compatible: bool,
    os_compatible: bool,
    arch_compatible: bool,
}

impl HostPlatformCheck {
    /// Returns the kernel release reported by `uname(2)`.
    pub fn kernel_release(&self) -> &str {
        &self.kernel_release
    }

    /// Returns whether all configured platform requirements are satisfied.
    pub fn is_compatible(&self) -> bool {
        self.kernel_compatible && self.os_compatible && self.arch_compatible
    }

    /// Returns whether the running kernel meets `min_kernel`.
    pub fn kernel_compatible(&self) -> bool {
        self.kernel_compatible
    }

    /// Returns whether the launcher target OS matches the bundle.
    pub fn os_compatible(&self) -> bool {
        self.os_compatible
    }

    /// Returns whether the launcher target architecture matches the bundle.
    pub fn arch_compatible(&self) -> bool {
        self.arch_compatible
    }
}

/// Failure to inspect the current host platform.
#[derive(Debug)]
pub enum HostPlatformError {
    /// `uname(2)` failed.
    Uname(io::Error),
    /// The kernel returned an unrecognizable release.
    InvalidKernelRelease(String),
}

impl fmt::Display for HostPlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uname(error) => write!(formatter, "failed to query host platform: {error}"),
            Self::InvalidKernelRelease(release) => {
                write!(formatter, "invalid host kernel release '{release}'")
            }
        }
    }
}

impl Error for HostPlatformError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Uname(error) => Some(error),
            Self::InvalidKernelRelease(_) => None,
        }
    }
}

/// Compares the running host with the bundle's declared platform.
pub fn check_host_platform(
    required: &PlatformConfig,
) -> Result<HostPlatformCheck, HostPlatformError> {
    // SAFETY: `uname` initializes the complete `utsname` structure on success.
    let mut name = unsafe { std::mem::zeroed::<libc::utsname>() };
    // SAFETY: `name` points to writable storage of the exact type expected.
    if unsafe { libc::uname(&mut name) } != 0 {
        return Err(HostPlatformError::Uname(io::Error::last_os_error()));
    }
    // SAFETY: a successful `uname` returns NUL-terminated fields.
    let release = unsafe { CStr::from_ptr(name.release.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let current_kernel = KernelVersion::parse_release(&release)?;
    let required_kernel = KernelVersion::parse_required(&required.min_kernel)
        .expect("BundleConfig::parse validates min_kernel");

    Ok(HostPlatformCheck {
        kernel_release: release,
        kernel_compatible: current_kernel >= required_kernel,
        os_compatible: matches!(required.os, PlatformOs::Linux),
        arch_compatible: cfg!(target_arch = "x86_64")
            && matches!(required.arch, PlatformArch::X86_64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders_kernel_versions() {
        let old = KernelVersion::parse_required("4.19").unwrap();
        let current = KernelVersion::parse_required("6.1.12").unwrap();

        assert!(current > old);
        assert_eq!(old.to_string(), "4.19.0");
        assert_eq!(
            KernelVersion::parse_release("6.12.9-arch1-1").unwrap(),
            KernelVersion::parse_required("6.12.9").unwrap()
        );
    }

    #[test]
    fn checks_the_current_linux_host() {
        let check = check_host_platform(&PlatformConfig {
            os: PlatformOs::Linux,
            arch: PlatformArch::X86_64,
            min_kernel: "1.0".to_string(),
        })
        .unwrap();

        assert!(check.kernel_compatible());
        assert!(check.os_compatible());
        assert_eq!(check.arch_compatible(), cfg!(target_arch = "x86_64"));
    }
}
