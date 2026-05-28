//! Host preflight checks for runtime-backed helper operations.

use crate::{error::RuntimeError, idmap::MbuildIdmap};
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(crate) fn preflight_local_helper_runtime(idmap: &MbuildIdmap) -> Result<(), RuntimeError> {
    check_local_helper_runtime_preflight(idmap, &HostPreflightProbe)
}

fn check_local_helper_runtime_preflight(
    idmap: &MbuildIdmap,
    probe: &impl PreflightProbe,
) -> Result<(), RuntimeError> {
    let mut failures = Vec::new();

    check_idmap(idmap, &mut failures);
    check_user_namespace_sysctl(probe, &mut failures);
    check_command_in_path(probe, "newuidmap", &mut failures);
    check_command_in_path(probe, "newgidmap", &mut failures);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::Preflight(failures.join("; ")))
    }
}

fn check_idmap(idmap: &MbuildIdmap, failures: &mut Vec<String>) {
    if idmap.subuid_count() == 0 {
        failures.push("mbuild idmap has empty subuid range".to_string());
    }
    if idmap.subgid_count() == 0 {
        failures.push("mbuild idmap has empty subgid range".to_string());
    }
}

fn check_user_namespace_sysctl(probe: &impl PreflightProbe, failures: &mut Vec<String>) {
    let max_user_namespaces = Path::new("/proc/sys/user/max_user_namespaces");
    match probe.read_to_string(max_user_namespaces) {
        Ok(value) => check_positive_sysctl(
            max_user_namespaces,
            &value,
            "unprivileged user namespaces",
            failures,
        ),
        Err(error) => failures.push(format!(
            "failed to read user namespace sysctl '{}': {error}",
            max_user_namespaces.display()
        )),
    }

    let unprivileged_clone = Path::new("/proc/sys/kernel/unprivileged_userns_clone");
    match probe.path_kind(unprivileged_clone) {
        Ok(PathKind::Missing) => {}
        Ok(PathKind::File) => match probe.read_to_string(unprivileged_clone) {
            Ok(value) => check_positive_sysctl(
                unprivileged_clone,
                &value,
                "unprivileged user namespace clone",
                failures,
            ),
            Err(error) => failures.push(format!(
                "failed to read user namespace clone sysctl '{}': {error}",
                unprivileged_clone.display()
            )),
        },
        Ok(kind) => failures.push(format!(
            "user namespace clone sysctl '{}' is not a file ({kind})",
            unprivileged_clone.display()
        )),
        Err(error) => failures.push(format!(
            "failed to inspect user namespace clone sysctl '{}': {error}",
            unprivileged_clone.display()
        )),
    }
}

fn check_positive_sysctl(path: &Path, value: &str, label: &str, failures: &mut Vec<String>) {
    match value.trim().parse::<u64>() {
        Ok(value) if value > 0 => {}
        Ok(_) => failures.push(format!("{label} disabled: '{}' is 0", path.display())),
        Err(error) => failures.push(format!(
            "failed to parse {label} sysctl '{}': {error}",
            path.display()
        )),
    }
}

fn check_command_in_path(probe: &impl PreflightProbe, name: &str, failures: &mut Vec<String>) {
    match probe.command_in_path(name) {
        Ok(true) => {}
        Ok(false) => failures.push(format!("{name} not found in PATH")),
        Err(error) => failures.push(format!("failed to inspect PATH for {name}: {error}")),
    }
}

trait PreflightProbe {
    fn path_kind(&self, path: &Path) -> io::Result<PathKind>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn command_in_path(&self, name: &str) -> io::Result<bool>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Missing,
    File,
    Directory,
    Socket,
    Other,
}

impl std::fmt::Display for PathKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Missing => "missing",
            Self::File => "file",
            Self::Directory => "directory",
            Self::Socket => "socket",
            Self::Other => "other",
        };
        formatter.write_str(name)
    }
}

struct HostPreflightProbe;

impl PreflightProbe for HostPreflightProbe {
    fn path_kind(&self, path: &Path) -> io::Result<PathKind> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_dir() {
                    Ok(PathKind::Directory)
                } else if file_type.is_file() {
                    Ok(PathKind::File)
                } else if is_socket(&file_type) {
                    Ok(PathKind::Socket)
                } else {
                    Ok(PathKind::Other)
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(PathKind::Missing),
            Err(error) => Err(error),
        }
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn command_in_path(&self, name: &str) -> io::Result<bool> {
        let Some(path) = env::var_os("PATH") else {
            return Ok(false);
        };
        for dir in env::split_paths(&path) {
            let candidate = dir.join(name);
            match fs::metadata(&candidate) {
                Ok(metadata)
                    if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 =>
                {
                    return Ok(true);
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
}

#[cfg(unix)]
fn is_socket(file_type: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;

    file_type.is_socket()
}

#[cfg(not(unix))]
fn is_socket(_: &fs::FileType) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn local_preflight_rejects_disabled_user_namespaces() {
        let mut probe = FakeProbe::complete();
        probe.files.insert(
            "/proc/sys/user/max_user_namespaces".to_string(),
            "0\n".to_string(),
        );
        probe.files.insert(
            "/proc/sys/kernel/unprivileged_userns_clone".to_string(),
            "0\n".to_string(),
        );

        let error = check_local_helper_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("unprivileged user namespaces disabled"));
        assert!(message.contains("unprivileged user namespace clone disabled"));
    }

    #[test]
    fn local_preflight_accepts_userns_and_newidmap_helpers() {
        check_local_helper_runtime_preflight(&test_idmap(), &FakeProbe::complete()).unwrap();
    }

    #[test]
    fn local_preflight_reports_missing_newidmap_helpers() {
        let mut probe = FakeProbe::complete();
        probe.commands.clear();

        let error = check_local_helper_runtime_preflight(&test_idmap(), &probe).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("newuidmap not found"));
        assert!(message.contains("newgidmap not found"));
    }

    #[test]
    fn preflight_reports_zero_test_idmap_ranges() {
        let probe = FakeProbe::complete();
        let idmap = MbuildIdmap::for_tests(1000, 1000, 100000, 0, 200000, 0);

        let error = check_local_helper_runtime_preflight(&idmap, &probe).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("empty subuid range"));
        assert!(message.contains("empty subgid range"));
    }

    fn test_idmap() -> MbuildIdmap {
        MbuildIdmap::for_tests(1000, 1000, 100000, 65536, 200000, 65536)
    }

    #[derive(Debug, Default)]
    struct FakeProbe {
        kinds: HashMap<String, PathKind>,
        files: HashMap<String, String>,
        commands: HashSet<String>,
    }

    impl FakeProbe {
        fn complete() -> Self {
            let mut probe = Self::default();
            probe.kinds.insert(
                "/proc/sys/user/max_user_namespaces".to_string(),
                PathKind::File,
            );
            probe.kinds.insert(
                "/proc/sys/kernel/unprivileged_userns_clone".to_string(),
                PathKind::File,
            );
            probe.files.insert(
                "/proc/sys/user/max_user_namespaces".to_string(),
                "1024\n".to_string(),
            );
            probe.files.insert(
                "/proc/sys/kernel/unprivileged_userns_clone".to_string(),
                "1\n".to_string(),
            );
            probe.commands.insert("newuidmap".to_string());
            probe.commands.insert("newgidmap".to_string());
            probe
        }
    }

    impl PreflightProbe for FakeProbe {
        fn path_kind(&self, path: &Path) -> io::Result<PathKind> {
            Ok(self
                .kinds
                .get(&path.display().to_string())
                .copied()
                .unwrap_or(PathKind::Missing))
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(&path.display().to_string())
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake file"))
        }

        fn command_in_path(&self, name: &str) -> io::Result<bool> {
            Ok(self.commands.contains(name))
        }
    }
}
