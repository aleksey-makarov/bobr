use std::env;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum FsUtilError {
    Io(String),
    InvalidInput(String),
}

impl fmt::Display for FsUtilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) | Self::InvalidInput(message) => write!(f, "{message}"),
        }
    }
}

pub fn temp_root_dir(root_dir: &str) -> Result<PathBuf, FsUtilError> {
    if root_dir.is_empty() {
        return Err(FsUtilError::InvalidInput(
            "root_dir must not be empty".to_string(),
        ));
    }

    let cwd = env::current_dir()
        .map_err(|error| FsUtilError::Io(format!("failed to get current directory: {error}")))?;
    let path = cwd.join(root_dir).join("tmp");
    fs::create_dir_all(&path).map_err(|error| {
        FsUtilError::Io(format!(
            "failed to create temp root directory '{}': {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

pub fn recreate_empty_dir_force(path: &Path) -> Result<(), FsUtilError> {
    if path.exists() {
        if path.is_dir() {
            remove_dir_force(path)?;
        } else {
            fs::remove_file(path).map_err(|error| {
                FsUtilError::Io(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        FsUtilError::Io(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

pub fn recreate_empty_dir(path: &Path) -> Result<(), FsUtilError> {
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path).map_err(|error| {
                FsUtilError::Io(format!(
                    "failed to remove previous directory '{}': {error}",
                    path.display()
                ))
            })?;
        } else {
            fs::remove_file(path).map_err(|error| {
                FsUtilError::Io(format!(
                    "failed to remove previous file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }

    fs::create_dir_all(path).map_err(|error| {
        FsUtilError::Io(format!(
            "failed to create directory '{}': {error}",
            path.display()
        ))
    })
}

pub fn remove_dir_force(path: &Path) -> Result<(), FsUtilError> {
    if !path.exists() {
        return Ok(());
    }
    make_tree_writable(path)?;
    fs::remove_dir_all(path).map_err(|error| {
        FsUtilError::Io(format!(
            "failed to remove directory '{}': {error}",
            path.display()
        ))
    })
}

pub fn write_atomic(path: &Path, content: &str) -> Result<(), FsUtilError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            FsUtilError::Io(format!(
                "invalid file name for atomic write path '{}'",
                path.display()
            ))
        })?;

    let tmp_name = format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        current_epoch_nanos()?
    );
    let tmp_path = path.with_file_name(tmp_name);

    fs::write(&tmp_path, content).map_err(|error| {
        FsUtilError::Io(format!(
            "failed to write temporary file '{}': {error}",
            tmp_path.display()
        ))
    })?;

    fs::rename(&tmp_path, path).map_err(|error| {
        FsUtilError::Io(format!(
            "failed to move temporary file '{}' to '{}': {error}",
            tmp_path.display(),
            path.display()
        ))
    })
}

pub fn current_epoch_nanos() -> Result<u128, FsUtilError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| FsUtilError::Io(format!("system time before UNIX_EPOCH: {error}")))
}

#[cfg(unix)]
fn make_tree_writable(path: &Path) -> Result<(), FsUtilError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        FsUtilError::Io(format!(
            "failed to inspect path '{}': {error}",
            path.display()
        ))
    })?;

    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_dir() {
        let mode = metadata.permissions().mode();
        let desired = mode | 0o700;
        if desired != mode {
            fs::set_permissions(path, fs::Permissions::from_mode(desired)).map_err(|error| {
                FsUtilError::Io(format!(
                    "failed to adjust permissions for '{}': {error}",
                    path.display()
                ))
            })?;
        }

        for entry in fs::read_dir(path).map_err(|error| {
            FsUtilError::Io(format!(
                "failed to read directory '{}': {error}",
                path.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                FsUtilError::Io(format!(
                    "failed to read directory entry in '{}': {error}",
                    path.display()
                ))
            })?;
            make_tree_writable(&entry.path())?;
        }
    }

    Ok(())
}

#[cfg(not(unix))]
fn make_tree_writable(path: &Path) -> Result<(), FsUtilError> {
    let _ = path;
    Ok(())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn remove_dir_force_does_not_chmod_hardlinked_files() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"data").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o555)).unwrap();

        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::hard_link(&source, tree.join("hardlink")).unwrap();
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o500)).unwrap();

        remove_dir_force(&tree).unwrap();

        assert!(!tree.exists());
        assert_eq!(
            fs::metadata(&source).unwrap().permissions().mode() & 0o7777,
            0o555
        );
    }
}
