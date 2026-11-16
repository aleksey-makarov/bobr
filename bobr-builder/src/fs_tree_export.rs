use crate::BuilderError;
use crate::plain_tree_compose::{PlainTreeComposeFunction, PlainTreeComposeInput, PlainTreeOp};
use crate::{BuildContext, BuilderInputs, InputSpec, TypedBuilder};
use bobr_core::BuildLogLevel;
use bobr_runtime::runtime::Runtime;
use bobr_store::fs_tree::{FsTree, FsTreeEntry, FsTreeManifest};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const EXPORT_ROOT_DIR_NAME: &str = "fs-tree-export-root";

/// Configuration for [`FsTreeExportBuilder`]: an ordered list of copy commands.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FsTreeExportConfig {
    /// Copy commands, applied in order. Must be non-empty.
    pub copies: Vec<CopyCommand>,
}

/// One export copy command.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopyCommand {
    /// A glob (or literal path) matched against the input fs-tree's paths.
    pub from: String,
    /// Destination inside the output object. For a literal `from` naming a
    /// single file or symlink, this is the exact output path (allowing rename).
    /// Otherwise it is a directory into which matches are placed, preserving
    /// each match's path relative to the glob's literal base (for a literal
    /// directory, relative to that directory).
    pub to: String,
}

/// Exports selected entries out of an fs-tree (the `input` manifest) into a
/// plain object directory owned `0:0`.
///
/// This is the inverse of `FsTreeImport`: it reads the manifest, pulls matched
/// regular files from `fs-files` by content hash, recreates matched symlinks,
/// and writes them to the output at the configured paths. Because fs-files
/// carry the logical ownership and mode of their entries (readable only with
/// privilege) and a plain object must be single-owner, the copy runs in a
/// namespace runtime function as root.
#[derive(Debug)]
pub struct FsTreeExportBuilder;

static FS_TREE_EXPORT_SPEC: InputSpec = InputSpec {
    required_inputs: &["input"],
    optional_inputs: &[],
    allow_extra_inputs: false,
};

impl TypedBuilder for FsTreeExportBuilder {
    type Config = FsTreeExportConfig;

    fn tag(&self) -> &'static str {
        "FsTreeExport"
    }

    fn impl_version(&self) -> &'static str {
        // Unified runtime composition and privileged destination validation.
        "2"
    }

    fn spec(&self) -> &'static InputSpec {
        &FS_TREE_EXPORT_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<PathBuf, BuilderError> {
        build_fs_tree_export(config, inputs, cx)
    }
}

fn build_fs_tree_export(
    config: FsTreeExportConfig,
    inputs: BuilderInputs,
    cx: &mut BuildContext,
) -> Result<PathBuf, BuilderError> {
    if config.copies.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "FsTreeExport requires at least one copy command".to_string(),
        ));
    }

    // The `input` is a non-`_` fs-tree object: its object path is the manifest.
    let manifest_path = inputs.required("input")?.clone();
    let manifest = FsTreeManifest::read_canonical(&manifest_path).map_err(|error| {
        BuilderError::InvalidRecipe(format!(
            "failed to read fs-tree manifest '{}': {error}",
            manifest_path.display()
        ))
    })?;
    let fs_tree = cx.fs_tree();

    // Resolve the copy plan in-process (pure over the manifest): match entries,
    // compute destinations, and resolve each file's fs-files source path. The
    // privileged reads/writes/chown happen in the runtime function.
    let ops = resolve_export_plan(&manifest, &fs_tree, &config.copies)?;

    let output_root = cx.temp_dir.join(EXPORT_ROOT_DIR_NAME);
    if output_root.exists() {
        return Err(BuilderError::ExecutionFailed(format!(
            "FsTreeExport output path already exists: '{}'",
            output_root.display()
        )));
    }

    cx.log_event(
        BuildLogLevel::Info,
        "export",
        format!("exporting {} entr{} into a plain object", ops.len(), {
            if ops.len() == 1 { "y" } else { "ies" }
        }),
    );

    cx.runtime()
        .run(
            &PlainTreeComposeFunction,
            PlainTreeComposeInput {
                output_root: output_root.clone(),
                ops,
                cancellation_fifo: cx.cancellation_token().fifo_path().to_path_buf(),
            },
        )
        .map_err(|error| BuilderError::ExecutionFailed(error.to_string()))?
        .into_builder_result()?;

    Ok(output_root)
}

/// Resolves all copy commands into a flat, validated list of write operations.
fn resolve_export_plan(
    manifest: &FsTreeManifest,
    fs_tree: &FsTree,
    copies: &[CopyCommand],
) -> Result<Vec<PlainTreeOp>, BuilderError> {
    let mut ops = Vec::new();
    let mut destinations = BTreeSet::<String>::new();

    for command in copies {
        let matches = plan_command(manifest, &command.from, &command.to)?;
        if matches.is_empty() {
            return Err(BuilderError::InvalidRecipe(format!(
                "FsTreeExport copy '{}' matched no files or symlinks",
                command.from
            )));
        }
        for (entry, dest) in matches {
            validate_output_rel_path(&dest)?;
            if !destinations.insert(dest.clone()) {
                return Err(BuilderError::InvalidRecipe(format!(
                    "FsTreeExport writes two entries to the same destination '{dest}'"
                )));
            }
            let op = match entry {
                FsTreeEntry::File { hash, .. } => {
                    let source = fs_tree.fs_file_path(*hash).map_err(|error| {
                        BuilderError::ExecutionFailed(format!(
                            "failed to resolve fs-file for '{dest}': {error}"
                        ))
                    })?;
                    PlainTreeOp::CopyFile { source, dest }
                }
                FsTreeEntry::Symlink { target, .. } => PlainTreeOp::Symlink {
                    target: target.clone(),
                    dest,
                },
                FsTreeEntry::Directory { .. } => continue,
            };
            ops.push(op);
        }
    }

    Ok(ops)
}

/// Matches one command against the manifest and pairs each matched file/symlink
/// entry with its output-relative destination. Directory entries never produce
/// operations (parent directories are created implicitly).
fn plan_command<'a>(
    manifest: &'a FsTreeManifest,
    from: &str,
    to: &str,
) -> Result<Vec<(&'a FsTreeEntry, String)>, BuilderError> {
    let mut matches = Vec::new();

    if is_glob(from) {
        let matcher = Glob::new(from)
            .map_err(|error| {
                BuilderError::InvalidRecipe(format!("invalid FsTreeExport glob '{from}': {error}"))
            })?
            .compile_matcher();
        let base = glob_literal_base(from);
        for entry in manifest.entries() {
            let path = entry.path();
            if path.is_empty() || !matcher.is_match(path) {
                continue;
            }
            if is_copyable(entry) {
                matches.push((entry, join_dest(to, strip_base(path, &base))));
            }
        }
    } else {
        // Literal path: an exact single entry (file/symlink -> `to`) or a
        // directory (recursive; descendants placed relative to it under `to`).
        let exact = manifest.entries().iter().find(|entry| entry.path() == from);
        match exact {
            Some(FsTreeEntry::Directory { .. }) | None if has_descendants(manifest, from) => {
                let prefix = format!("{from}/");
                for entry in manifest.entries() {
                    let path = entry.path();
                    if let Some(rel) = path.strip_prefix(&prefix)
                        && is_copyable(entry)
                    {
                        matches.push((entry, join_dest(to, rel)));
                    }
                }
            }
            Some(entry) if is_copyable(entry) => {
                matches.push((entry, to.to_string()));
            }
            _ => {}
        }
    }

    Ok(matches)
}

fn is_copyable(entry: &FsTreeEntry) -> bool {
    matches!(
        entry,
        FsTreeEntry::File { .. } | FsTreeEntry::Symlink { .. }
    )
}

fn has_descendants(manifest: &FsTreeManifest, path: &str) -> bool {
    let prefix = format!("{path}/");
    manifest
        .entries()
        .iter()
        .any(|entry| entry.path().starts_with(&prefix))
}

/// Whether `pattern` uses any glob metacharacter; otherwise it is a literal path.
fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', ']', '{', '}'])
}

/// The leading directory prefix of a glob before its first metacharacter
/// component (e.g. `usr/lib` for `usr/lib/*.so`, `usr` for `usr/**/*.so`, `""`
/// for `*.so`).
fn glob_literal_base(pattern: &str) -> String {
    let mut base = Vec::new();
    for component in pattern.split('/') {
        if is_glob(component) {
            break;
        }
        base.push(component);
    }
    base.join("/")
}

fn strip_base<'a>(path: &'a str, base: &str) -> &'a str {
    if base.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{base}/")).unwrap_or(path)
    }
}

fn join_dest(to: &str, rel: &str) -> String {
    match (to.is_empty(), rel.is_empty()) {
        (true, _) => rel.to_string(),
        (false, true) => to.to_string(),
        (false, false) => format!("{to}/{rel}"),
    }
}

/// Validates that a destination is a safe relative path that stays inside the
/// output root.
fn validate_output_rel_path(path: &str) -> Result<(), BuilderError> {
    if path.is_empty() {
        return Err(BuilderError::InvalidRecipe(
            "FsTreeExport destination must not be empty".to_string(),
        ));
    }
    if Path::new(path).is_absolute() {
        return Err(BuilderError::InvalidRecipe(format!(
            "FsTreeExport destination '{path}' must be relative"
        )));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(BuilderError::InvalidRecipe(format!(
                "FsTreeExport destination '{path}' must not contain empty, '.', or '..' segments"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::store_fs_tree;
    use bobr_store::fs_tree::FsFileHash;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn file_hash(byte: u8) -> FsFileHash {
        FsFileHash::from_str(&format!("{byte:02x}").repeat(32)).unwrap()
    }

    fn sample_manifest() -> FsTreeManifest {
        FsTreeManifest::from_entries(vec![
            FsTreeEntry::directory("", 0, 0, 0o755),
            FsTreeEntry::directory("boot", 0, 0, 0o755),
            FsTreeEntry::file("boot/bzImage", file_hash(1)),
            FsTreeEntry::directory("usr", 0, 0, 0o755),
            FsTreeEntry::directory("usr/lib", 0, 0, 0o755),
            FsTreeEntry::file("usr/lib/libfoo.so.1", file_hash(2)),
            FsTreeEntry::file("usr/lib/libbar.so.1", file_hash(3)),
            FsTreeEntry::symlink("usr/lib/libfoo.so", 0, 0, "libfoo.so.1"),
        ])
        .unwrap()
    }

    fn dests(matches: &[(&FsTreeEntry, String)]) -> Vec<String> {
        let mut out = matches
            .iter()
            .map(|(_, dest)| dest.clone())
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    #[test]
    fn registered_builder_exports_independent_files_symlinks_and_canonical_directories() {
        use std::collections::BTreeMap;
        use std::fs;
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("usr/bin")).unwrap();
        fs::set_permissions(source.join("usr"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(source.join("usr/bin"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::write(source.join("usr/bin/tool"), b"tool").unwrap();
        fs::set_permissions(
            source.join("usr/bin/tool"),
            fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        symlink("tool", source.join("usr/bin/link")).unwrap();
        let fs_tree = store_fs_tree(temp.path());
        let manifest_path = temp.path().join("manifest");
        let manifest = fs_tree.intern_tree(source).unwrap();
        manifest.write_canonical(&manifest_path).unwrap();
        let source_hash = manifest
            .entries()
            .iter()
            .find_map(|entry| match entry {
                FsTreeEntry::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .unwrap();
        let source_file = fs_tree.fs_file_path(source_hash).unwrap();
        let build_dir = temp.path().join("build");
        fs::create_dir(&build_dir).unwrap();
        let mut cx = BuildContext::with_noop_logger(build_dir, fs_tree);
        let builder = crate::BUILDERS
            .iter()
            .copied()
            .find(|builder| builder.tag() == "FsTreeExport")
            .unwrap();
        assert_eq!(builder.impl_version(), "2");
        let plan = builder
            .plan(serde_json::json!({"copies": [{"from": "usr", "to": "export"}]}))
            .unwrap();
        let output = plan
            .build(
                BuilderInputs::new(BTreeMap::from([("input".into(), manifest_path)])),
                &mut cx,
            )
            .unwrap();
        assert_eq!(fs::read(output.join("export/bin/tool")).unwrap(), b"tool");
        assert_eq!(
            fs::read_link(output.join("export/bin/link")).unwrap(),
            Path::new("tool")
        );
        assert_ne!(
            fs::metadata(source_file).unwrap().ino(),
            fs::metadata(output.join("export/bin/tool")).unwrap().ino()
        );
        let owner = fs::metadata(&output).unwrap();
        for directory in ["", "export", "export/bin"] {
            let metadata = fs::metadata(output.join(directory)).unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o755);
            assert_eq!((metadata.uid(), metadata.gid()), (owner.uid(), owner.gid()));
        }
        let metadata = fs::metadata(output.join("export/bin/tool")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o751);
        assert_eq!((metadata.uid(), metadata.gid()), (owner.uid(), owner.gid()));
        let link = fs::symlink_metadata(output.join("export/bin/link")).unwrap();
        assert_eq!((link.uid(), link.gid()), (owner.uid(), owner.gid()));
    }

    #[test]
    fn builder_propagates_typed_cancellation_without_staging_output() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("file"), b"file").unwrap();
        let fs_tree = store_fs_tree(temp.path());
        let manifest = temp.path().join("manifest");
        fs_tree
            .intern_tree(source)
            .unwrap()
            .write_canonical(&manifest)
            .unwrap();
        let build_dir = temp.path().join("build");
        std::fs::create_dir(&build_dir).unwrap();
        let mut cx = BuildContext::with_noop_logger(build_dir.clone(), fs_tree);
        cx.cancellation_token().cancel();
        let result = FsTreeExportBuilder.build_typed(
            FsTreeExportConfig {
                copies: vec![CopyCommand {
                    from: "file".into(),
                    to: "file".into(),
                }],
            },
            BuilderInputs::new(std::collections::BTreeMap::from([(
                "input".into(),
                manifest,
            )])),
            &mut cx,
        );
        assert!(matches!(result, Err(BuilderError::Cancelled(_))));
        assert_eq!(std::fs::read_dir(build_dir).unwrap().count(), 0);
    }

    #[test]
    fn literal_file_is_renamed_to_exact_destination() {
        let manifest = sample_manifest();
        let matches = plan_command(&manifest, "boot/bzImage", "bzImage").unwrap();
        assert_eq!(dests(&matches), vec!["bzImage".to_string()]);
    }

    #[test]
    fn glob_places_matches_relative_to_literal_base() {
        let manifest = sample_manifest();
        let matches = plan_command(&manifest, "usr/lib/*.so.1", "libs").unwrap();
        assert_eq!(
            dests(&matches),
            vec![
                "libs/libbar.so.1".to_string(),
                "libs/libfoo.so.1".to_string()
            ]
        );
    }

    #[test]
    fn literal_directory_exports_its_subtree() {
        let manifest = sample_manifest();
        let matches = plan_command(&manifest, "usr/lib", "L").unwrap();
        // Files and the symlink, but not the directory entry itself.
        assert_eq!(
            dests(&matches),
            vec![
                "L/libbar.so.1".to_string(),
                "L/libfoo.so".to_string(),
                "L/libfoo.so.1".to_string(),
            ]
        );
    }

    #[test]
    fn glob_base_is_prefix_before_first_metacharacter() {
        assert_eq!(glob_literal_base("usr/lib/*.so"), "usr/lib");
        assert_eq!(glob_literal_base("usr/**/*.so"), "usr");
        assert_eq!(glob_literal_base("*.so"), "");
    }

    #[test]
    fn resolve_rejects_duplicate_destinations() {
        let temp = tempdir().unwrap();
        let fs_tree = store_fs_tree(temp.path());
        let manifest = sample_manifest();
        let error = resolve_export_plan(
            &manifest,
            &fs_tree,
            &[
                CopyCommand {
                    from: "boot/bzImage".into(),
                    to: "k".into(),
                },
                CopyCommand {
                    from: "usr/lib/libfoo.so.1".into(),
                    to: "k".into(),
                },
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("same destination"), "{error}");
    }

    #[test]
    fn resolve_rejects_command_that_matches_nothing() {
        let temp = tempdir().unwrap();
        let fs_tree = store_fs_tree(temp.path());
        let manifest = sample_manifest();
        let error = resolve_export_plan(
            &manifest,
            &fs_tree,
            &[CopyCommand {
                from: "no/such/path".into(),
                to: "x".into(),
            }],
        )
        .unwrap_err();
        assert!(error.to_string().contains("matched no files"), "{error}");
    }

    #[test]
    fn destination_escaping_the_root_is_rejected() {
        assert!(validate_output_rel_path("../escape").is_err());
        assert!(validate_output_rel_path("/abs").is_err());
        assert!(validate_output_rel_path("ok/path").is_ok());
    }
}
