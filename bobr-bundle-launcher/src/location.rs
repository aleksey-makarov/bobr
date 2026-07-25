//! HostBundle location derived from the running launcher's executable path.

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Executable name expected for the HostBundle launcher.
pub const LAUNCHER_BINARY_NAME: &str = "bobr-bundle-launcher";
/// Directory containing the real launcher executable in a HostBundle.
pub const BUNDLE_LIBEXEC_DIR: &str = "libexec";
/// Runtime configuration file at the root of a HostBundle.
pub const BUNDLE_CONFIG_NAME: &str = "bundle.toml";

const PROC_SELF_EXE: &str = "/proc/self/exe";

/// Paths derived from a launcher's validated position inside a HostBundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleLocation {
    root: PathBuf,
    launcher: PathBuf,
}

impl BundleLocation {
    /// Returns the root directory of the HostBundle.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the resolved path of the real launcher executable.
    pub fn launcher(&self) -> &Path {
        &self.launcher
    }

    /// Returns the expected path of the HostBundle runtime configuration.
    pub fn config(&self) -> PathBuf {
        self.root.join(BUNDLE_CONFIG_NAME)
    }
}

/// Failure to derive a HostBundle location from the launcher executable.
#[derive(Debug)]
pub enum BundleLocationError {
    /// `/proc/self/exe` could not be resolved.
    ReadCurrentExecutable(io::Error),
    /// The resolved executable path was not absolute.
    RelativeLauncherPath(PathBuf),
    /// The resolved executable did not have the expected file name.
    UnexpectedLauncherName(PathBuf),
    /// The resolved executable was not directly inside `libexec`.
    UnexpectedLauncherDirectory(PathBuf),
}

impl fmt::Display for BundleLocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadCurrentExecutable(error) => {
                write!(formatter, "failed to resolve {PROC_SELF_EXE}: {error}")
            }
            Self::RelativeLauncherPath(path) => write!(
                formatter,
                "launcher path '{}' is not absolute",
                path.display()
            ),
            Self::UnexpectedLauncherName(path) => write!(
                formatter,
                "launcher path '{}' does not end with '{LAUNCHER_BINARY_NAME}'",
                path.display()
            ),
            Self::UnexpectedLauncherDirectory(path) => write!(
                formatter,
                "launcher path '{}' is not inside a '{BUNDLE_LIBEXEC_DIR}' directory",
                path.display()
            ),
        }
    }
}

impl Error for BundleLocationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadCurrentExecutable(error) => Some(error),
            Self::RelativeLauncherPath(_)
            | Self::UnexpectedLauncherName(_)
            | Self::UnexpectedLauncherDirectory(_) => None,
        }
    }
}

/// Resolves the current executable through procfs and locates its HostBundle.
///
/// The guaranteed runtime layout is
/// `<bundle>/libexec/bobr-bundle-launcher`. Invoking the launcher through a
/// symlink is supported because `/proc/self/exe` resolves to the real
/// executable.
pub fn locate_current_bundle() -> Result<BundleLocation, BundleLocationError> {
    let launcher =
        fs::read_link(PROC_SELF_EXE).map_err(BundleLocationError::ReadCurrentExecutable)?;
    locate_bundle_from_launcher(&launcher)
}

/// Locates a HostBundle from a resolved launcher executable path.
///
/// This function performs only structural path validation. The caller is
/// responsible for opening and validating `bundle.toml`.
pub fn locate_bundle_from_launcher(launcher: &Path) -> Result<BundleLocation, BundleLocationError> {
    if !launcher.is_absolute() {
        return Err(BundleLocationError::RelativeLauncherPath(
            launcher.to_path_buf(),
        ));
    }
    if launcher.file_name() != Some(OsStr::new(LAUNCHER_BINARY_NAME)) {
        return Err(BundleLocationError::UnexpectedLauncherName(
            launcher.to_path_buf(),
        ));
    }

    let Some(libexec) = launcher.parent() else {
        return Err(BundleLocationError::UnexpectedLauncherDirectory(
            launcher.to_path_buf(),
        ));
    };
    if libexec.file_name() != Some(OsStr::new(BUNDLE_LIBEXEC_DIR)) {
        return Err(BundleLocationError::UnexpectedLauncherDirectory(
            launcher.to_path_buf(),
        ));
    }

    let Some(root) = libexec.parent() else {
        return Err(BundleLocationError::UnexpectedLauncherDirectory(
            launcher.to_path_buf(),
        ));
    };

    Ok(BundleLocation {
        root: root.to_path_buf(),
        launcher: launcher.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_bundle_from_expected_launcher_path() {
        let launcher = Path::new("/store/object/libexec/bobr-bundle-launcher");

        let location = locate_bundle_from_launcher(launcher).unwrap();

        assert_eq!(location.root(), Path::new("/store/object"));
        assert_eq!(location.launcher(), launcher);
        assert_eq!(
            location.config(),
            PathBuf::from("/store/object/bundle.toml")
        );
    }

    #[test]
    fn accepts_bundle_root_at_filesystem_root() {
        let launcher = Path::new("/libexec/bobr-bundle-launcher");

        let location = locate_bundle_from_launcher(launcher).unwrap();

        assert_eq!(location.root(), Path::new("/"));
    }

    #[test]
    fn rejects_relative_launcher_path() {
        let error =
            locate_bundle_from_launcher(Path::new("libexec/bobr-bundle-launcher")).unwrap_err();

        assert!(matches!(
            error,
            BundleLocationError::RelativeLauncherPath(_)
        ));
    }

    #[test]
    fn rejects_unexpected_launcher_name() {
        let error =
            locate_bundle_from_launcher(Path::new("/store/object/libexec/other")).unwrap_err();

        assert!(matches!(
            error,
            BundleLocationError::UnexpectedLauncherName(_)
        ));
    }

    #[test]
    fn rejects_launcher_outside_libexec() {
        let error =
            locate_bundle_from_launcher(Path::new("/store/object/bin/bobr-bundle-launcher"))
                .unwrap_err();

        assert!(matches!(
            error,
            BundleLocationError::UnexpectedLauncherDirectory(_)
        ));
    }
}
