use mbuild_core::BuilderError;
#[cfg(test)]
use mbuild_core::InitramfsEntrySource;
use mbuild_core::{ComposedFsTree, ComposedFsTreeEntry, FsTreeComposeInput, FsTreeEntry};
use mbuild_runtime::FsTreeArchiveEntrySource;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::io;
#[cfg(test)]
use std::io::Write;
use std::path::{Component, Path};

pub(super) fn archive_sources(
    input_label: &str,
    inputs: &[FsTreeComposeInput],
    composed: &ComposedFsTree,
) -> Result<Vec<FsTreeArchiveEntrySource>, BuilderError> {
    composed
        .manifest()
        .entries()
        .iter()
        .zip(composed.entries())
        .map(
            |(manifest_entry, composed_entry)| match (manifest_entry, composed_entry) {
                (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory) => {
                    Ok(FsTreeArchiveEntrySource::Directory)
                }
                (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { source_path }) => {
                    let (input_index, rel_path) =
                        locate_archive_source(input_label, inputs, source_path)?;
                    Ok(FsTreeArchiveEntrySource::File {
                        input_index,
                        path: rel_path,
                    })
                }
                (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                    Ok(FsTreeArchiveEntrySource::Symlink)
                }
                _ => Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                ))),
            },
        )
        .collect()
}

pub(super) fn locate_archive_source(
    input_label: &str,
    inputs: &[FsTreeComposeInput],
    source_path: &Path,
) -> Result<(usize, String), BuilderError> {
    for (index, input) in inputs.iter().enumerate() {
        if let Ok(rel_path) = source_path.strip_prefix(&input.root_dir) {
            let rel_path = rel_path_to_manifest_string(rel_path).ok_or_else(|| {
                BuilderError::ExecutionFailed(format!(
                    "fs-tree {input_label} source path '{}' is not representable as a manifest path",
                    source_path.display()
                ))
            })?;
            if rel_path.is_empty() {
                return Err(BuilderError::ExecutionFailed(format!(
                    "fs-tree {input_label} source path '{}' points at an input root, expected a file",
                    source_path.display()
                )));
            }
            return Ok((index, rel_path));
        }
    }
    Err(BuilderError::ExecutionFailed(format!(
        "fs-tree {input_label} source path '{}' is not under any {input_label} input root",
        source_path.display()
    )))
}

pub(super) fn rel_path_to_manifest_string(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
pub(super) fn write_composed_fs_tree_tar_host(
    composed: &ComposedFsTree,
    output_tar: &Path,
) -> Result<(), BuilderError> {
    let file = fs::File::create(output_tar).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create EROFS source tar '{}': {error}",
            output_tar.display()
        ))
    })?;
    write_composed_fs_tree_tar_stream(file, composed)
}

#[cfg(test)]
pub(super) fn write_composed_fs_tree_initramfs_host(
    composed: &ComposedFsTree,
    output_initramfs: &Path,
) -> Result<(), BuilderError> {
    let file = fs::File::create(output_initramfs).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to create initramfs '{}': {error}",
            output_initramfs.display()
        ))
    })?;
    let sources = initramfs_host_sources(composed)?;
    mbuild_core::write_newc_initramfs(file, composed.manifest().entries(), &sources)
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))
}

#[cfg(test)]
pub(super) fn initramfs_host_sources(
    composed: &ComposedFsTree,
) -> Result<Vec<InitramfsEntrySource>, BuilderError> {
    composed
        .manifest()
        .entries()
        .iter()
        .zip(composed.entries())
        .map(
            |(manifest_entry, composed_entry)| match (manifest_entry, composed_entry) {
                (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory) => {
                    Ok(InitramfsEntrySource::Directory)
                }
                (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { source_path }) => {
                    Ok(InitramfsEntrySource::File {
                        path: source_path.clone(),
                    })
                }
                (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                    Ok(InitramfsEntrySource::Symlink)
                }
                _ => Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                ))),
            },
        )
        .collect()
}

#[cfg(test)]
pub(super) fn write_composed_fs_tree_tar_stream<W: Write>(
    writer: W,
    composed: &ComposedFsTree,
) -> Result<(), BuilderError> {
    let mut tar = tar::Builder::new(io::BufWriter::new(writer));
    tar.mode(tar::HeaderMode::Deterministic);

    for (manifest_entry, composed_entry) in
        composed.manifest().entries().iter().zip(composed.entries())
    {
        if manifest_entry.path().is_empty() {
            continue;
        }
        match (manifest_entry, composed_entry) {
            (FsTreeEntry::Directory { .. }, ComposedFsTreeEntry::Directory) => {
                append_erofs_tar_directory(&mut tar, manifest_entry)?
            }
            (FsTreeEntry::File { .. }, ComposedFsTreeEntry::File { source_path }) => {
                append_erofs_tar_file(&mut tar, manifest_entry, source_path)?
            }
            (FsTreeEntry::Symlink { .. }, ComposedFsTreeEntry::Symlink { .. }) => {
                append_erofs_tar_symlink(&mut tar, manifest_entry)?
            }
            _ => {
                return Err(BuilderError::ExecutionFailed(format!(
                    "merged fs-tree entry for '{}' does not match manifest kind",
                    manifest_entry.path()
                )));
            }
        }
    }

    let mut writer = tar.into_inner().map_err(|error| {
        BuilderError::ExecutionFailed(format!("failed to finalize EROFS source tar: {error}"))
    })?;
    writer.flush().map_err(|error| {
        BuilderError::ExecutionFailed(format!("failed to flush EROFS source tar: {error}"))
    })?;
    Ok(())
}

#[cfg(test)]
fn append_erofs_tar_directory<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
) -> Result<(), BuilderError> {
    let FsTreeEntry::Directory {
        path,
        uid,
        gid,
        mode,
    } = entry
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: expected directory manifest entry".to_string(),
        ));
    };
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(*mode);
    header.set_mtime(0);
    header.set_size(0);
    header.set_cksum();
    tar.append_data(&mut header, format!("{path}/"), io::empty())
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to append EROFS source tar directory '{path}': {error}"
            ))
        })
}

#[cfg(test)]
fn append_erofs_tar_file<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
    source_path: &Path,
) -> Result<(), BuilderError> {
    let FsTreeEntry::File {
        path,
        uid,
        gid,
        mode,
        ..
    } = entry
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: expected file manifest entry".to_string(),
        ));
    };
    let metadata = fs::metadata(source_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to stat EROFS source file '{}': {error}",
            source_path.display()
        ))
    })?;
    let mut file = fs::File::open(source_path).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to open EROFS source file '{}': {error}",
            source_path.display()
        ))
    })?;
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(*mode);
    header.set_mtime(0);
    header.set_size(metadata.len());
    header.set_cksum();
    tar.append_data(&mut header, path, &mut file)
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to append EROFS source tar file '{path}': {error}"
            ))
        })
}

#[cfg(test)]
fn append_erofs_tar_symlink<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &FsTreeEntry,
) -> Result<(), BuilderError> {
    let FsTreeEntry::Symlink {
        path,
        uid,
        gid,
        target,
        ..
    } = entry
    else {
        return Err(BuilderError::ExecutionFailed(
            "internal error: expected symlink manifest entry".to_string(),
        ));
    };
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_uid(*uid as u64);
    header.set_gid(*gid as u64);
    header.set_mode(0o777);
    header.set_mtime(0);
    header.set_size(0);
    header.set_link_name(target).map_err(|error| {
        BuilderError::ExecutionFailed(format!(
            "failed to encode EROFS source tar symlink target '{target}' for '{path}': {error}"
        ))
    })?;
    header.set_cksum();
    tar.append_data(&mut header, path, io::empty())
        .map_err(|error| {
            BuilderError::ExecutionFailed(format!(
                "failed to append EROFS source tar symlink '{path}': {error}"
            ))
        })
}
