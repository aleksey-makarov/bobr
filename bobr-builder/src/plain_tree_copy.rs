//! Privileged, symlink-safe copying into independent plain directory objects.

use bobr_runtime::runtime::{RuntimeError, RuntimeFunction};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown, symlink};
use std::path::{Path, PathBuf};

/// Runtime function used by HostBundle composition.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlainTreeCopyFunction;

/// Complete copy request for one new plain directory object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlainTreeCopyInput {
    pub(crate) output_root: PathBuf,
    pub(crate) copies: Vec<PlainTreeCopy>,
}

/// One independently copied input path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "kebab-case")]
pub(crate) enum PlainTreeCopy {
    /// Recursively copy a real directory without following symlinks.
    Tree { source: PathBuf, dest: String },
    /// Copy one regular file to an exact destination.
    File { source: PathBuf, dest: String },
}

impl RuntimeFunction for PlainTreeCopyFunction {
    type Input = PlainTreeCopyInput;
    type Output = ();

    fn name(&self) -> &'static str {
        "plain-tree-copy"
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, RuntimeError> {
        copy_plain_tree(input).map_err(RuntimeError::new)
    }
}

fn copy_plain_tree(input: PlainTreeCopyInput) -> Result<(), String> {
    if input.output_root.exists() || fs::symlink_metadata(&input.output_root).is_ok() {
        return Err(format!(
            "plain-tree copy output already exists: '{}'",
            input.output_root.display()
        ));
    }
    fs::create_dir(&input.output_root).map_err(|error| {
        format!(
            "failed to create plain-tree output '{}': {error}",
            input.output_root.display()
        )
    })?;

    for copy in input.copies {
        match copy {
            PlainTreeCopy::Tree { source, dest } => {
                let destination = checked_destination(&input.output_root, &dest)?;
                copy_tree(&source, &destination)?;
            }
            PlainTreeCopy::File { source, dest } => {
                let destination = checked_destination(&input.output_root, &dest)?;
                copy_regular_file(&source, &destination)?;
            }
        }
    }

    normalize_owner(&input.output_root)
}

fn checked_destination(root: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty() || Path::new(relative).is_absolute() {
        return Err(format!(
            "plain-tree destination '{relative}' must be a non-empty relative path"
        ));
    }
    if relative
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!(
            "plain-tree destination '{relative}' contains an unsafe path component"
        ));
    }
    Ok(root.join(relative))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("failed to inspect '{}': {error}", source.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "plain-tree source '{}' is not a real directory",
            source.display()
        ));
    }
    create_parent(destination)?;
    fs::create_dir(destination).map_err(|error| {
        format!(
            "failed to create copied directory '{}': {error}",
            destination.display()
        )
    })?;
    copy_directory_contents(source, destination)?;
    set_mode(destination, metadata.mode())
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<(), String> {
    let mut entries = fs::read_dir(source)
        .map_err(|error| format!("failed to read '{}': {error}", source.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read '{}': {error}", source.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| format!("failed to inspect '{}': {error}", source_path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            fs::create_dir(&destination_path).map_err(|error| {
                format!(
                    "failed to create copied directory '{}': {error}",
                    destination_path.display()
                )
            })?;
            copy_directory_contents(&source_path, &destination_path)?;
            set_mode(&destination_path, metadata.mode())?;
        } else if file_type.is_file() {
            copy_regular_file_with_metadata(&source_path, &destination_path, &metadata)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&source_path).map_err(|error| {
                format!(
                    "failed to read symlink '{}': {error}",
                    source_path.display()
                )
            })?;
            symlink(&target, &destination_path).map_err(|error| {
                format!(
                    "failed to recreate symlink '{}' -> '{}': {error}",
                    destination_path.display(),
                    target.display()
                )
            })?;
        } else {
            return Err(format!(
                "unsupported special file in HostBundle input: '{}'",
                source_path.display()
            ));
        }
    }
    Ok(())
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("failed to inspect '{}': {error}", source.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "plain-tree file source '{}' is not a regular file",
            source.display()
        ));
    }
    create_parent(destination)?;
    copy_regular_file_with_metadata(source, destination, &metadata)
}

fn copy_regular_file_with_metadata(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
) -> Result<(), String> {
    fs::copy(source, destination).map_err(|error| {
        format!(
            "failed to copy '{}' to '{}': {error}",
            source.display(),
            destination.display()
        )
    })?;
    set_mode(destination, metadata.mode())
}

fn create_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("path '{}' has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create directory '{}': {error}", parent.display()))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|error| format!("failed to chmod '{}': {error}", path.display()))
}

fn normalize_owner(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
        entries.sort();
        for entry in entries {
            normalize_owner(&entry)?;
        }
    }

    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    lchown(path, Some(uid), Some(gid))
        .map_err(|error| format!("failed to normalize owner of '{}': {error}", path.display()))
}

/// Removes write, setuid, and setgid bits from a completed plain directory tree.
pub(crate) fn make_tree_read_only(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
        entries.sort();
        for entry in entries {
            make_tree_read_only(&entry)?;
        }
    }
    if !metadata.file_type().is_symlink() {
        let mode = metadata.permissions().mode() & 0o7777 & !0o6222;
        set_mode(path, mode)?;
    }
    Ok(())
}

/// Verifies the final permission invariant without following symlinks.
pub(crate) fn verify_tree_read_only(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect '{}': {error}", path.display()))?;
    if !metadata.file_type().is_symlink() && metadata.permissions().mode() & 0o6222 != 0 {
        return Err(format!(
            "published HostBundle path '{}' retains write, setuid, or setgid bits",
            path.display()
        ));
    }
    if metadata.file_type().is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
        entries.sort();
        for entry in entries {
            verify_tree_read_only(&entry)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_runtime::runtime::RuntimeFunction;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::tempdir;

    #[test]
    fn copies_independent_files_and_recreates_symlinks() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("tool"), b"payload").unwrap();
        fs::set_permissions(source.join("tool"), fs::Permissions::from_mode(0o751)).unwrap();
        symlink("tool", source.join("tool-link")).unwrap();
        let output = temp.path().join("output");

        PlainTreeCopyFunction
            .call(PlainTreeCopyInput {
                output_root: output.clone(),
                copies: vec![PlainTreeCopy::Tree {
                    source: source.clone(),
                    dest: "root".to_string(),
                }],
            })
            .unwrap();

        assert_eq!(fs::read(output.join("root/tool")).unwrap(), b"payload");
        assert_eq!(
            fs::read_link(output.join("root/tool-link")).unwrap(),
            PathBuf::from("tool")
        );
        assert_eq!(
            fs::metadata(source.join("tool")).unwrap().ino()
                == fs::metadata(output.join("root/tool")).unwrap().ino(),
            false
        );
        assert_eq!(
            fs::metadata(output.join("root/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o751
        );
    }

    #[test]
    fn refuses_symlink_roots_and_special_files() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = temp.path().join("linked");
        symlink(&real, &linked).unwrap();

        let error = PlainTreeCopyFunction
            .call(PlainTreeCopyInput {
                output_root: temp.path().join("out-link"),
                copies: vec![PlainTreeCopy::Tree {
                    source: linked,
                    dest: "root".to_string(),
                }],
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("not a real directory"),
            "{error}"
        );

        let source = temp.path().join("special");
        fs::create_dir(&source).unwrap();
        let fifo = source.join("fifo");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) };
        assert_eq!(result, 0);
        let error = PlainTreeCopyFunction
            .call(PlainTreeCopyInput {
                output_root: temp.path().join("out-special"),
                copies: vec![PlainTreeCopy::Tree {
                    source,
                    dest: "root".to_string(),
                }],
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("unsupported special file"),
            "{error}"
        );
    }

    #[test]
    fn rejects_unsafe_destinations() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();

        let error = PlainTreeCopyFunction
            .call(PlainTreeCopyInput {
                output_root: temp.path().join("out"),
                copies: vec![PlainTreeCopy::Tree {
                    source,
                    dest: "../escape".to_string(),
                }],
            })
            .unwrap_err();
        assert!(error.to_string().contains("unsafe path component"));
    }

    #[test]
    fn finalization_removes_write_setuid_and_setgid_bits() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("bundle");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("tool"), b"tool").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o2775)).unwrap();
        fs::set_permissions(root.join("tool"), fs::Permissions::from_mode(0o6755)).unwrap();
        symlink("tool", root.join("link")).unwrap();

        make_tree_read_only(&root).unwrap();
        verify_tree_read_only(&root).unwrap();

        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
            0o555
        );
        assert_eq!(
            fs::metadata(root.join("tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
    }
}
