use crate::origin::{OriginContext, OriginHandler, OriginSpec, ParsedOrigin};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tar::Archive;

static PATH_ORIGIN_SPEC: OriginSpec = OriginSpec { tag: "Path" };

#[derive(Debug)]
pub(super) struct PathOriginHandler;

#[derive(Debug, Clone)]
struct PathOrigin {
    path: PathBuf,
    unpack: bool,
}

impl OriginHandler for PathOriginHandler {
    fn spec(&self) -> &'static OriginSpec {
        &PATH_ORIGIN_SPEC
    }

    fn parse(
        &self,
        mut object: Map<String, Value>,
        field_path: &str,
    ) -> Result<Box<dyn ParsedOrigin>, String> {
        let kind = take_string(&mut object, field_path, "tag")?;
        debug_assert_eq!(kind, "Path");
        let path_value = PathBuf::from(take_string(&mut object, field_path, "path")?);
        validate_absolute_source_path(&path_value, &format!("{field_path}.path"))?;
        let unpack = take_optional_bool(&mut object, field_path, "unpack")?.unwrap_or(false);
        if !object.is_empty() {
            return Err(format!(
                "{field_path}: unexpected fields: {}",
                object.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(Box::new(PathOrigin {
            path: path_value,
            unpack,
        }))
    }
}

impl ParsedOrigin for PathOrigin {
    fn spec(&self) -> &'static OriginSpec {
        &PATH_ORIGIN_SPEC
    }

    fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String> {
        // Local and fast: one milestone, no per-chunk ticks or cancellation.
        if self.unpack {
            cx.milestone(format!("unpacking {}", self.path.display()));
            materialize_path_source_tar(cx.temp_root, &self.path)
        } else {
            cx.milestone(format!("copying {}", self.path.display()));
            materialize_path_source_direct(cx.temp_root, &self.path)
        }
    }

    fn clone_box(&self) -> Box<dyn ParsedOrigin> {
        Box::new(self.clone())
    }
}

fn take_string(object: &mut Map<String, Value>, path: &str, field: &str) -> Result<String, String> {
    let value = object
        .remove(field)
        .ok_or_else(|| format!("{path}: missing required field '{field}'"))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{path}.{field}: expected string"))
}

fn take_optional_bool(
    object: &mut Map<String, Value>,
    path: &str,
    field: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = object.remove(field) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("{path}.{field}: expected boolean"))
}

fn validate_absolute_source_path(path: &Path, field_path: &str) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{field_path}: path must not be empty"));
    }
    if !path.is_absolute() {
        return Err(format!("{field_path}: expected absolute path"));
    }
    Ok(())
}

fn materialize_path_source_direct(temp_root: &Path, source_path: &Path) -> Result<PathBuf, String> {
    let source_meta = fs::metadata(source_path).map_err(|error| {
        format!(
            "failed to inspect source path '{}': {error}",
            source_path.display()
        )
    })?;
    let staged_path = temp_root.join("staged");
    if source_meta.is_dir() {
        copy_dir_recursive(source_path, &staged_path)?;
    } else if source_meta.is_file() {
        if let Some(parent) = staged_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create staging parent '{}': {error}",
                    parent.display()
                )
            })?;
        }
        fs::copy(source_path, &staged_path).map_err(|error| {
            format!(
                "failed to copy source file '{}' to '{}': {error}",
                source_path.display(),
                staged_path.display()
            )
        })?;
    } else {
        return Err(format!(
            "source path '{}' must be a regular file or directory",
            source_path.display()
        ));
    }
    Ok(staged_path)
}

fn materialize_path_source_tar(temp_root: &Path, source_path: &Path) -> Result<PathBuf, String> {
    let file = fs::File::open(source_path).map_err(|error| {
        format!(
            "failed to open tar source '{}': {error}",
            source_path.display()
        )
    })?;
    let staged_path = temp_root.join("staged");
    fs::create_dir_all(&staged_path).map_err(|error| {
        format!(
            "failed to create tar staging dir '{}': {error}",
            staged_path.display()
        )
    })?;
    let mut archive = Archive::new(file);
    archive.unpack(&staged_path).map_err(|error| {
        format!(
            "failed to unpack tar source '{}' into '{}': {error}",
            source_path.display(),
            staged_path.display()
        )
    })?;
    Ok(staged_path)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst)
        .map_err(|error| format!("failed to create directory '{}': {error}", dst.display()))?;
    for entry in fs::read_dir(src)
        .map_err(|error| format!("failed to read directory '{}': {error}", src.display()))?
    {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read directory entry under '{}': {error}",
                src.display()
            )
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to inspect file type for '{}': {error}",
                src_path.display()
            )
        })?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path).map_err(|error| {
                format!(
                    "failed to copy file '{}' to '{}': {error}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        } else if file_type.is_symlink() {
            copy_symlink(&src_path, &dst_path)?;
        } else {
            return Err(format!(
                "unsupported filesystem entry '{}' in directory source",
                src_path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> Result<(), String> {
    use std::os::unix::fs as unix_fs;
    let target = fs::read_link(src)
        .map_err(|error| format!("failed to read symlink '{}': {error}", src.display()))?;
    unix_fs::symlink(&target, dst).map_err(|error| {
        format!(
            "failed to create symlink '{}' -> '{}': {error}",
            dst.display(),
            target.display()
        )
    })
}

#[cfg(not(unix))]
fn copy_symlink(_src: &Path, _dst: &Path) -> Result<(), String> {
    Err("copying symlink entries is unsupported on this platform".to_string())
}
