use std::env;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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

    let cwd = env::current_dir().map_err(|error| {
        FsUtilError::Io(format!("failed to get current directory: {error}"))
    })?;
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
        FsUtilError::Io(format!("failed to create directory '{}': {error}", path.display()))
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

#[cfg(unix)]
fn make_tree_writable(path: &Path) -> Result<(), FsUtilError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        FsUtilError::Io(format!("failed to inspect path '{}': {error}", path.display()))
    })?;

    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    let mode = metadata.permissions().mode();
    let desired = if metadata.is_dir() {
        mode | 0o700
    } else {
        mode | 0o600
    };

    if desired != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(desired)).map_err(|error| {
            FsUtilError::Io(format!(
                "failed to adjust permissions for '{}': {error}",
                path.display()
            ))
        })?;
    }

    if metadata.is_dir() {
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
