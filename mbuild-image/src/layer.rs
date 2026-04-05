use crate::oci::sha256_hex;
use flate2::Compression;
use flate2::write::GzEncoder;

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug)]
pub enum LayerError {
    Io(String),
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayerError::Io(msg) => f.write_str(msg),
        }
    }
}

pub struct LayerBlobs {
    /// gzip-compressed tar bytes; this is what goes in blobs/sha256/<digest>
    pub compressed: Vec<u8>,
    /// sha256(uncompressed tar) — the "diffID" field in the OCI config rootfs
    pub diff_id: String,
}

/// Collect all entries from multiple binary-output directories into a single
/// deterministic gzip-compressed tar layer blob.
///
/// Rules for determinism:
/// - entries are sorted by path across all inputs (lexicographic)
/// - mtime = 0, uid = 0, gid = 0, uname = "", gname = ""
/// - symlinks are preserved as-is; directories are included explicitly
///
/// Conflict detection (additive-only) is NOT done here; callers are responsible.
pub fn create_layer(binary_outputs: &[&Path]) -> Result<LayerBlobs, LayerError> {
    // Collect all (relative_path, source_abs_path) pairs, sorted.
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    for &dir in binary_outputs {
        collect_entries(dir, dir, &mut entries)?;
    }
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    entries.dedup_by(|(a, _), (b, _)| a == b);

    // Resolve symlink/directory conflicts.
    //
    // When one binary output has `lib64` as a symlink (→ `usr/lib64`) and another
    // has `lib64/` as a real directory with files inside, we must:
    //   1. Keep the symlink entry.
    //   2. Drop the directory entry (`lib64/`).
    //   3. Rewrite paths under the directory (`lib64/foo`) through the symlink
    //      target (`usr/lib64/foo`), so that the files land in the right place
    //      and parent directories are guaranteed to exist before the files.
    //
    // Without step 3, `storage-untar` would try to open `lib64/foo` which resolves
    // through the symlink to `usr/lib64/foo`, but `usr/lib64/` may not yet exist
    // at that point in the extraction sequence.

    // Build a map of symlink_rel → resolved_target_prefix for symlinks that have
    // a conflicting directory entry.
    let mut symlink_rewrite: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    {
        // First pass: collect all symlinks and their targets.
        let all_symlinks: std::collections::HashMap<String, String> = entries
            .iter()
            .filter_map(|(rel, abs)| {
                let is_symlink = fs::symlink_metadata(abs)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                if !is_symlink {
                    return None;
                }
                let target = fs::read_link(abs).ok()?;
                let target_str = target.to_str()?.to_string();
                Some((rel.clone(), target_str))
            })
            .collect();

        // Collect directory entries that conflict with a symlink.
        let conflicting_dirs: std::collections::HashSet<String> = entries
            .iter()
            .filter_map(|(rel, _)| {
                if rel.ends_with('/') {
                    let name = &rel[..rel.len() - 1];
                    if all_symlinks.contains_key(name) {
                        return Some(name.to_string());
                    }
                }
                None
            })
            .collect();

        // Build the rewrite map only for symlinks that actually have a conflict.
        for sym_rel in &conflicting_dirs {
            if let Some(raw_target) = all_symlinks.get(sym_rel) {
                let resolved = resolve_symlink_target(sym_rel, raw_target);
                symlink_rewrite.insert(sym_rel.clone(), resolved);
            }
        }
    }

    // Apply path rewrites: `lib64/foo` → `usr/lib64/foo`.
    if !symlink_rewrite.is_empty() {
        for (rel, _abs) in entries.iter_mut() {
            for (sym_rel, target_prefix) in &symlink_rewrite {
                let prefix = format!("{sym_rel}/");
                if rel.starts_with(&prefix) {
                    let suffix = &rel[prefix.len()..];
                    *rel = format!("{target_prefix}/{suffix}");
                    break;
                }
            }
        }
        // Re-sort and re-dedup after rewrites.
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        entries.dedup_by(|(a, _), (b, _)| a == b);
    }

    // Remove directory entries that are now superseded by a symlink.
    let symlink_paths: std::collections::HashSet<String> = entries
        .iter()
        .filter(|(_, abs)| {
            fs::symlink_metadata(abs)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        })
        .map(|(rel, _)| rel.clone())
        .collect();
    entries.retain(|(rel, _)| {
        if rel.ends_with('/') {
            !symlink_paths.contains(&rel[..rel.len() - 1])
        } else {
            true
        }
    });

    // Build uncompressed tar in memory and simultaneously hash it.
    let mut uncompressed: Vec<u8> = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut uncompressed);
        tar.follow_symlinks(false);
        for (rel_path, abs_path) in &entries {
            append_entry(&mut tar, rel_path, abs_path)?;
        }
        tar.finish()
            .map_err(|e| LayerError::Io(format!("failed to finish tar: {e}")))?;
    }

    let diff_id = format!("sha256:{}", sha256_hex(&uncompressed));

    // Gzip compress.
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&uncompressed)
        .map_err(|e| LayerError::Io(format!("failed to gzip compress: {e}")))?;
    let compressed = gz
        .finish()
        .map_err(|e| LayerError::Io(format!("failed to finish gzip: {e}")))?;

    Ok(LayerBlobs {
        compressed,
        diff_id,
    })
}

fn collect_entries(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<(), LayerError> {
    let metadata = fs::symlink_metadata(current)
        .map_err(|e| LayerError::Io(format!("failed to stat '{}': {e}", current.display())))?;

    // Build relative path from root to current (without leading /).
    let rel = if current == root {
        // root itself; we add its children, not root
        // Iterate children
        for entry in fs::read_dir(current).map_err(|e| {
            LayerError::Io(format!("failed to read dir '{}': {e}", current.display()))
        })? {
            let entry = entry.map_err(|e| {
                LayerError::Io(format!(
                    "failed to read entry in '{}': {e}",
                    current.display()
                ))
            })?;
            collect_entries(root, &entry.path(), out)?;
        }
        return Ok(());
    } else {
        current
            .strip_prefix(root)
            .map_err(|_| {
                LayerError::Io(format!(
                    "path '{}' is not under root '{}'",
                    current.display(),
                    root.display()
                ))
            })?
            .to_string_lossy()
            .replace('\\', "/")
    };

    if metadata.is_symlink() {
        out.push((rel, current.to_path_buf()));
        return Ok(());
    }

    if metadata.is_dir() {
        out.push((format!("{rel}/"), current.to_path_buf()));
        for entry in fs::read_dir(current).map_err(|e| {
            LayerError::Io(format!("failed to read dir '{}': {e}", current.display()))
        })? {
            let entry = entry.map_err(|e| {
                LayerError::Io(format!(
                    "failed to read entry in '{}': {e}",
                    current.display()
                ))
            })?;
            collect_entries(root, &entry.path(), out)?;
        }
        return Ok(());
    }

    // Regular file.
    out.push((rel, current.to_path_buf()));
    Ok(())
}

fn append_entry(
    tar: &mut tar::Builder<impl std::io::Write>,
    rel_path: &str,
    abs_path: &Path,
) -> Result<(), LayerError> {
    let metadata = fs::symlink_metadata(abs_path)
        .map_err(|e| LayerError::Io(format!("failed to stat '{}': {e}", abs_path.display())))?;

    let mut header = tar::Header::new_gnu();
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("").ok();
    header.set_groupname("").ok();

    if metadata.is_symlink() {
        let target = fs::read_link(abs_path).map_err(|e| {
            LayerError::Io(format!(
                "failed to read symlink '{}': {e}",
                abs_path.display()
            ))
        })?;
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header
            .set_link_name(&target)
            .map_err(|e| LayerError::Io(format!("failed to set link name: {e}")))?;
        header.set_mode(0o777);
        header.set_cksum();
        tar.append_data(&mut header, rel_path, std::io::empty())
            .map_err(|e| LayerError::Io(format!("failed to append symlink '{rel_path}': {e}")))?;
        return Ok(());
    }

    if metadata.is_dir() {
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, rel_path, std::io::empty())
            .map_err(|e| LayerError::Io(format!("failed to append dir '{rel_path}': {e}")))?;
        return Ok(());
    }

    // Regular file.
    let file_bytes = fs::read(abs_path).map_err(|e| {
        LayerError::Io(format!("failed to read file '{}': {e}", abs_path.display()))
    })?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    };
    #[cfg(not(unix))]
    let mode = 0o644u32;

    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(file_bytes.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    tar.append_data(&mut header, rel_path, file_bytes.as_slice())
        .map_err(|e| LayerError::Io(format!("failed to append file '{rel_path}': {e}")))?;
    Ok(())
}

/// Resolve a symlink target relative to the directory containing the symlink.
///
/// `sym_rel`: the relative path of the symlink entry (e.g. `"lib64"`)
/// `raw_target`: the raw target string stored in the symlink (e.g. `"usr/lib64"` or `"/usr/lib64"`)
///
/// Returns the resolved relative path to use as the replacement prefix
/// (e.g. `"usr/lib64"`), with any `..` components collapsed.
fn resolve_symlink_target(sym_rel: &str, raw_target: &str) -> String {
    // Determine the directory that contains the symlink.
    let sym_dir = match sym_rel.rfind('/') {
        Some(pos) => &sym_rel[..pos],
        None => "",
    };

    // Build the candidate path: sym_dir / raw_target (absolute targets skip sym_dir).
    let candidate = if raw_target.starts_with('/') {
        raw_target[1..].to_string()
    } else if sym_dir.is_empty() {
        raw_target.to_string()
    } else {
        format!("{sym_dir}/{raw_target}")
    };

    // Collapse . and .. components.
    let mut parts: Vec<&str> = Vec::new();
    for component in candidate.split('/') {
        match component {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            c => parts.push(c),
        }
    }
    parts.join("/")
}
