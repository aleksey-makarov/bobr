use globset::{Glob, GlobMatcher};
use mbuild_core::{
    BuildContext, BuildLogLevel, BuilderError, BuilderInputObject, BuilderInputs, BuilderSpec,
    StagedBuildResult, TypedBuilder,
};
use serde::Deserialize;
use serde_json::Map;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

const OUTPUT_FILE_NAME: &str = "rootfs.ext4";
const ZERO_UUID: &str = "00000000-0000-0000-0000-000000000000";
const ZERO_HASH_SEED: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Debug)]
enum ComposeError {
    InvalidConfig(String),
    InputResolutionFailed(String),
    CompositionFailed(String),
    ExecutionFailed(String),
    FsFailed(String),
}

impl ComposeError {
    fn message(&self) -> &str {
        match self {
            Self::InvalidConfig(message)
            | Self::InputResolutionFailed(message)
            | Self::CompositionFailed(message)
            | Self::ExecutionFailed(message)
            | Self::FsFailed(message) => message,
        }
    }
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

type CResult<T> = Result<T, ComposeError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ext4RootfsConfig {
    size_mib: u64,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsConfig {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallMeta {
    rules: Vec<InstallRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRule {
    path: String,
    attrs: InstallAttrs,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallAttrs {
    #[serde(default)]
    uid: Option<u32>,
    #[serde(default)]
    gid: Option<u32>,
    #[serde(default)]
    directory_mode: Option<u32>,
    #[serde(default)]
    regular_file_mode: Option<u32>,
    #[serde(default)]
    executable_file_mode: Option<u32>,
    #[serde(default)]
    symlink_mode: Option<u32>,
}

#[derive(Debug)]
struct CompiledInstallRule {
    pattern: String,
    matcher: GlobMatcher,
    attrs: InstallAttrs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryKind {
    File {
        source_path: PathBuf,
        executable: bool,
    },
    Directory,
    Symlink {
        target: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContributionEntry {
    rel_path: String,
    kind: EntryKind,
    mode: u32,
    uid: u32,
    gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootEntry {
    rel_path: String,
    kind: EntryKind,
    mode: u32,
    uid: u32,
    gid: u32,
}

#[derive(Debug, Clone)]
struct RootManifest {
    entries: Vec<RootEntry>,
}

pub struct Ext4RootfsBuilder;
pub struct RootfsBuilder;

static EXT4_ROOTFS_SPEC: BuilderSpec = BuilderSpec {
    tag: "Ext4Rootfs",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

static ROOTFS_SPEC: BuilderSpec = BuilderSpec {
    tag: "Rootfs",
    required_inputs: &[],
    optional_inputs: &[],
    allow_extra_inputs: true,
};

impl TypedBuilder for Ext4RootfsBuilder {
    type Config = Ext4RootfsConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &EXT4_ROOTFS_SPEC
    }

    fn build_typed(
        &self,
        config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        validate_config(&config).map_err(map_error)?;
        let trees = inputs
            .extras(&EXT4_ROOTFS_SPEC)
            .map(|(_, object)| object)
            .collect::<Vec<_>>();
        if trees.is_empty() {
            return Err(map_error(ComposeError::InvalidConfig(
                "Ext4Rootfs builder requires at least one directory input".to_string(),
            )));
        }

        cx.log_event(
            BuildLogLevel::Info,
            "prepare",
            format!("composing {} install tree input(s)", trees.len()),
        );

        let manifest = compose_install_manifest(&trees).map_err(map_error)?;
        let output_path = cx.temp_dir.join(OUTPUT_FILE_NAME);
        cx.log_event(
            BuildLogLevel::Info,
            "materialize",
            format!("writing ext4 rootfs image '{}'", output_path.display()),
        );
        write_ext4_image(&manifest, &output_path, &config).map_err(map_error)?;

        Ok(StagedBuildResult {
            meta: Map::new(),
            staged_path: output_path,
            object_hash: None,
        })
    }
}

impl TypedBuilder for RootfsBuilder {
    type Config = RootfsConfig;

    fn spec(&self) -> &'static BuilderSpec {
        &ROOTFS_SPEC
    }

    fn build_typed(
        &self,
        _config: Self::Config,
        inputs: BuilderInputs,
        cx: &mut BuildContext,
    ) -> Result<StagedBuildResult, BuilderError> {
        let trees = inputs
            .extras(&ROOTFS_SPEC)
            .map(|(_, object)| object)
            .collect::<Vec<_>>();
        if trees.is_empty() {
            return Err(map_error(ComposeError::InvalidConfig(
                "Rootfs builder requires at least one directory input".to_string(),
            )));
        }

        cx.log_event(
            BuildLogLevel::Info,
            "prepare",
            format!("composing {} install tree input(s)", trees.len()),
        );

        let manifest = compose_install_manifest(&trees).map_err(map_error)?;
        let output_path = cx.temp_dir.join("rootfs");
        cx.log_event(
            BuildLogLevel::Info,
            "materialize",
            format!("writing rootfs directory '{}'", output_path.display()),
        );
        materialize_rootfs_directory(&manifest, &output_path).map_err(map_error)?;

        Ok(StagedBuildResult {
            meta: Map::new(),
            staged_path: output_path,
            object_hash: None,
        })
    }
}

fn validate_config(config: &Ext4RootfsConfig) -> CResult<()> {
    if config.size_mib == 0 {
        return Err(ComposeError::InvalidConfig(
            "size_mib must be greater than zero".to_string(),
        ));
    }
    if let Some(label) = &config.label
        && label.is_empty()
    {
        return Err(ComposeError::InvalidConfig(
            "label must not be empty when provided".to_string(),
        ));
    }
    Ok(())
}

fn compose_install_manifest(trees: &[&BuilderInputObject]) -> CResult<RootManifest> {
    let mut all_contributions = Vec::new();
    for (index, input) in trees.iter().enumerate() {
        validate_input_tree(index, input)?;
        let install = parse_install_meta(input)?;
        let compiled = compile_install_rules(&install.rules)?;
        let contribution = scan_install_tree(&input.object_path, &compiled)?;
        all_contributions.extend(contribution);
    }

    merge_contributions(all_contributions)
}

fn validate_input_tree(index: usize, input: &BuilderInputObject) -> CResult<()> {
    if !input.object_path.is_dir() {
        return Err(ComposeError::InputResolutionFailed(format!(
            "inputs[{index}] must resolve to a directory: {}",
            input.object_path.display()
        )));
    }
    Ok(())
}

fn parse_install_meta(input: &BuilderInputObject) -> CResult<InstallMeta> {
    let Some(value) = input.meta.get("install") else {
        return Err(ComposeError::InputResolutionFailed(format!(
            "input '{}' is missing meta.install",
            input.object_path.display()
        )));
    };
    serde_json::from_value::<InstallMeta>(value.clone()).map_err(|error| {
        ComposeError::InputResolutionFailed(format!(
            "input '{}' has invalid meta.install: {error}",
            input.object_path.display()
        ))
    })
}

fn compile_install_rules(rules: &[InstallRule]) -> CResult<Vec<CompiledInstallRule>> {
    if rules.is_empty() {
        return Err(ComposeError::InputResolutionFailed(
            "meta.install.rules must contain at least one rule".to_string(),
        ));
    }

    rules
        .iter()
        .map(|rule| {
            let glob = Glob::new(&rule.path).map_err(|error| {
                ComposeError::InputResolutionFailed(format!(
                    "invalid install rule pattern '{}': {error}",
                    rule.path
                ))
            })?;
            Ok(CompiledInstallRule {
                pattern: rule.path.clone(),
                matcher: glob.compile_matcher(),
                attrs: rule.attrs.clone(),
            })
        })
        .collect()
}

fn scan_install_tree(
    root: &Path,
    rules: &[CompiledInstallRule],
) -> CResult<Vec<ContributionEntry>> {
    let mut entries = Vec::new();
    scan_dir_recursive(root, root, rules, &mut entries)?;
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(entries)
}

fn scan_dir_recursive(
    root: &Path,
    current: &Path,
    rules: &[CompiledInstallRule],
    entries: &mut Vec<ContributionEntry>,
) -> CResult<()> {
    let mut children = fs::read_dir(current)
        .map_err(|error| {
            ComposeError::FsFailed(format!(
                "failed to read directory '{}': {error}",
                current.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            ComposeError::FsFailed(format!(
                "failed to enumerate directory '{}': {error}",
                current.display()
            ))
        })?;
    children.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    for child in children {
        let child_path = child.path();
        let rel = child_path
            .strip_prefix(root)
            .expect("child path should be under the scanned root");
        let rel_string = rel.to_string_lossy().replace('\\', "/");
        let metadata = fs::symlink_metadata(&child_path).map_err(|error| {
            ComposeError::FsFailed(format!(
                "failed to inspect path '{}': {error}",
                child_path.display()
            ))
        })?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            let attrs = resolve_install_attrs(&rel_string, rules)?;
            entries.push(ContributionEntry {
                rel_path: rel_string.clone(),
                kind: EntryKind::Directory,
                mode: attrs.directory_mode.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved directory_mode"
                    ))
                })?,
                uid: attrs.uid.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved uid"
                    ))
                })?,
                gid: attrs.gid.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved gid"
                    ))
                })?,
            });
            scan_dir_recursive(root, &child_path, rules, entries)?;
        } else if file_type.is_file() {
            let executable = (unix_mode(&metadata) & 0o111) != 0;
            let attrs = resolve_install_attrs(&rel_string, rules)?;
            entries.push(ContributionEntry {
                rel_path: rel_string.clone(),
                kind: EntryKind::File {
                    source_path: child_path,
                    executable,
                },
                mode: if executable {
                    attrs.executable_file_mode.ok_or_else(|| {
                        ComposeError::CompositionFailed(format!(
                            "path '{rel_string}' is missing resolved executable_file_mode"
                        ))
                    })?
                } else {
                    attrs.regular_file_mode.ok_or_else(|| {
                        ComposeError::CompositionFailed(format!(
                            "path '{rel_string}' is missing resolved regular_file_mode"
                        ))
                    })?
                },
                uid: attrs.uid.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved uid"
                    ))
                })?,
                gid: attrs.gid.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved gid"
                    ))
                })?,
            });
        } else if file_type.is_symlink() {
            let target = fs::read_link(&child_path).map_err(|error| {
                ComposeError::FsFailed(format!(
                    "failed to read symlink '{}': {error}",
                    child_path.display()
                ))
            })?;
            let attrs = resolve_install_attrs(&rel_string, rules)?;
            entries.push(ContributionEntry {
                rel_path: rel_string.clone(),
                kind: EntryKind::Symlink { target },
                mode: attrs.symlink_mode.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved symlink_mode"
                    ))
                })?,
                uid: attrs.uid.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved uid"
                    ))
                })?,
                gid: attrs.gid.ok_or_else(|| {
                    ComposeError::CompositionFailed(format!(
                        "path '{rel_string}' is missing resolved gid"
                    ))
                })?,
            });
        } else {
            return Err(ComposeError::CompositionFailed(format!(
                "unsupported file type in install tree: {}",
                child_path.display()
            )));
        }
    }

    Ok(())
}

fn resolve_install_attrs(rel_path: &str, rules: &[CompiledInstallRule]) -> CResult<InstallAttrs> {
    let mut resolved = InstallAttrs::default();
    let mut matched_any = false;
    for rule in rules {
        if install_rule_matches(rule, rel_path) {
            matched_any = true;
            if let Some(uid) = rule.attrs.uid {
                resolved.uid = Some(uid);
            }
            if let Some(gid) = rule.attrs.gid {
                resolved.gid = Some(gid);
            }
            if let Some(mode) = rule.attrs.directory_mode {
                resolved.directory_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.regular_file_mode {
                resolved.regular_file_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.executable_file_mode {
                resolved.executable_file_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.symlink_mode {
                resolved.symlink_mode = Some(mode);
            }
        }
    }
    if matched_any {
        Ok(resolved)
    } else {
        let known = rules
            .iter()
            .map(|rule| rule.pattern.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(ComposeError::CompositionFailed(format!(
            "path '{rel_path}' is not covered by any install rule (known patterns: {known})"
        )))
    }
}

fn install_rule_matches(rule: &CompiledInstallRule, rel_path: &str) -> bool {
    if rule.matcher.is_match(rel_path) {
        return true;
    }

    if let Some(prefix) = rule.pattern.strip_suffix("/**") {
        return rel_path == prefix;
    }

    false
}

fn merge_contributions(entries: Vec<ContributionEntry>) -> CResult<RootManifest> {
    let mut merged = BTreeMap::<String, RootEntry>::new();

    for entry in entries {
        match merged.get(&entry.rel_path) {
            None => {
                merged.insert(
                    entry.rel_path.clone(),
                    RootEntry {
                        rel_path: entry.rel_path,
                        kind: entry.kind,
                        mode: entry.mode,
                        uid: entry.uid,
                        gid: entry.gid,
                    },
                );
            }
            Some(existing) => {
                if is_matching_directory(existing, &entry) {
                    continue;
                }
                return Err(ComposeError::CompositionFailed(format!(
                    "path conflict at '{}': existing {:?}, new {:?}",
                    existing.rel_path, existing.kind, entry.kind
                )));
            }
        }
    }

    let mut final_entries = Vec::with_capacity(merged.len() + 1);
    final_entries.push(RootEntry {
        rel_path: String::new(),
        kind: EntryKind::Directory,
        mode: 0o755,
        uid: 0,
        gid: 0,
    });
    final_entries.extend(merged.into_values());

    Ok(RootManifest {
        entries: final_entries,
    })
}

fn is_matching_directory(existing: &RootEntry, new: &ContributionEntry) -> bool {
    matches!(
        (&existing.kind, &new.kind),
        (EntryKind::Directory, EntryKind::Directory)
    ) && existing.mode == new.mode
        && existing.uid == new.uid
        && existing.gid == new.gid
}

fn write_ext4_image(
    manifest: &RootManifest,
    output_path: &Path,
    config: &Ext4RootfsConfig,
) -> CResult<()> {
    let size_bytes = config
        .size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| ComposeError::InvalidConfig("size_mib is too large".to_string()))?;
    let output = fs::File::create(output_path).map_err(|error| {
        ComposeError::FsFailed(format!(
            "failed to create ext4 image '{}': {error}",
            output_path.display()
        ))
    })?;
    output.set_len(size_bytes).map_err(|error| {
        ComposeError::FsFailed(format!(
            "failed to resize ext4 image '{}': {error}",
            output_path.display()
        ))
    })?;

    let mut command = Command::new("mke2fs");
    command
        .arg("-q")
        .arg("-t")
        .arg("ext4")
        .arg("-d")
        .arg("-")
        .arg("-E")
        .arg("root_owner=0:0,lazy_itable_init=0,lazy_journal_init=0")
        .arg("-U")
        .arg(ZERO_UUID)
        .arg("-F");
    if let Some(label) = &config.label {
        command.arg("-L").arg(label);
    }
    command
        .arg(output_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("E2FSPROGS_FAKE_TIME", "0");

    let mut child = command.spawn().map_err(|error| {
        ComposeError::ExecutionFailed(format!("failed to spawn mke2fs: {error}"))
    })?;

    let stdin = child.stdin.take().ok_or_else(|| {
        ComposeError::ExecutionFailed("failed to open stdin for mke2fs".to_string())
    })?;
    let entries = manifest.entries.clone();
    let writer = thread::spawn(move || write_tar_stream(stdin, &entries));

    let status = child.wait().map_err(|error| {
        ComposeError::ExecutionFailed(format!("failed waiting for mke2fs: {error}"))
    })?;
    let writer_result = writer
        .join()
        .map_err(|_| ComposeError::ExecutionFailed("tar writer thread panicked".to_string()))?;
    writer_result?;

    if !status.success() {
        let stderr = child
            .stderr
            .take()
            .map(read_pipe_to_string)
            .transpose()
            .map_err(|error| {
                ComposeError::ExecutionFailed(format!("failed to read mke2fs stderr: {error}"))
            })?
            .unwrap_or_default();
        return Err(ComposeError::ExecutionFailed(format!(
            "mke2fs failed with status {status}: {stderr}"
        )));
    }

    normalize_ext4_hash_seed(output_path)?;
    rebuild_ext4_directory_indexes(output_path)?;
    normalize_ext4_timestamps(output_path)?;
    Ok(())
}

fn normalize_ext4_hash_seed(output_path: &Path) -> CResult<()> {
    let script = format!("ssv hash_seed {ZERO_HASH_SEED}\nclose -a\nquit\n");
    run_debugfs_script(output_path, &script, "normalizing ext4 hash seed")
}

fn normalize_ext4_timestamps(output_path: &Path) -> CResult<()> {
    run_debugfs_script(
        output_path,
        "ssv wtime 0\nssv lastcheck 0\nclose -a\nquit\n",
        "normalizing ext4 timestamps",
    )
}

fn run_debugfs_script(output_path: &Path, script: &str, purpose: &str) -> CResult<()> {
    let mut child = Command::new("debugfs")
        .arg("-w")
        .arg(output_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            ComposeError::ExecutionFailed(format!("failed to spawn debugfs: {error}"))
        })?;

    child
        .stdin
        .take()
        .ok_or_else(|| {
            ComposeError::ExecutionFailed("failed to open stdin for debugfs".to_string())
        })?
        .write_all(script.as_bytes())
        .map_err(|error| {
            ComposeError::ExecutionFailed(format!("failed to write debugfs script: {error}"))
        })?;

    let output = child.wait_with_output().map_err(|error| {
        ComposeError::ExecutionFailed(format!("failed waiting for debugfs: {error}"))
    })?;
    if !output.status.success() {
        return Err(ComposeError::ExecutionFailed(format!(
            "debugfs failed while {purpose}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn rebuild_ext4_directory_indexes(output_path: &Path) -> CResult<()> {
    let output = Command::new("e2fsck")
        .arg("-fyD")
        .arg(output_path)
        .env("E2FSPROGS_FAKE_TIME", "0")
        .output()
        .map_err(|error| {
            ComposeError::ExecutionFailed(format!("failed to spawn e2fsck: {error}"))
        })?;
    if !output.status.success() {
        return Err(ComposeError::ExecutionFailed(format!(
            "e2fsck failed while rebuilding ext4 directory indexes: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn write_tar_stream(writer: impl Write, entries: &[RootEntry]) -> CResult<()> {
    let buf = BufWriter::new(writer);
    let mut tar = tar::Builder::new(buf);
    tar.mode(tar::HeaderMode::Deterministic);

    for entry in entries {
        if entry.rel_path.is_empty() {
            continue;
        }
        match &entry.kind {
            EntryKind::Directory => append_directory(&mut tar, entry)?,
            EntryKind::File { source_path, .. } => append_file(&mut tar, entry, source_path)?,
            EntryKind::Symlink { target } => append_symlink(&mut tar, entry, target)?,
        }
    }

    let mut writer = tar.into_inner().map_err(|error| {
        ComposeError::ExecutionFailed(format!("failed to finalize tar stream: {error}"))
    })?;
    writer.flush().map_err(|error| {
        ComposeError::ExecutionFailed(format!("failed to flush tar stream: {error}"))
    })?;
    Ok(())
}

fn append_directory<W: Write>(tar: &mut tar::Builder<W>, entry: &RootEntry) -> CResult<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_mode(entry.mode as u32);
    header.set_uid(entry.uid as u64);
    header.set_gid(entry.gid as u64);
    header.set_mtime(0);
    header.set_size(0);
    header.set_cksum();
    tar.append_data(&mut header, format!("{}/", entry.rel_path), io::empty())
        .map_err(|error| {
            ComposeError::ExecutionFailed(format!(
                "failed to append directory '{}' to tar stream: {error}",
                entry.rel_path
            ))
        })
}

fn append_file<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &RootEntry,
    source_path: &Path,
) -> CResult<()> {
    let metadata = fs::metadata(source_path).map_err(|error| {
        ComposeError::FsFailed(format!(
            "failed to stat source file '{}': {error}",
            source_path.display()
        ))
    })?;
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(entry.mode as u32);
    header.set_uid(entry.uid as u64);
    header.set_gid(entry.gid as u64);
    header.set_mtime(0);
    header.set_size(metadata.len());
    header.set_cksum();
    let mut file = fs::File::open(source_path).map_err(|error| {
        ComposeError::FsFailed(format!(
            "failed to open source file '{}': {error}",
            source_path.display()
        ))
    })?;
    tar.append_data(&mut header, &entry.rel_path, &mut file)
        .map_err(|error| {
            ComposeError::ExecutionFailed(format!(
                "failed to append file '{}' to tar stream: {error}",
                entry.rel_path
            ))
        })
}

fn append_symlink<W: Write>(
    tar: &mut tar::Builder<W>,
    entry: &RootEntry,
    target: &Path,
) -> CResult<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_mode(entry.mode as u32);
    header.set_uid(entry.uid as u64);
    header.set_gid(entry.gid as u64);
    header.set_mtime(0);
    header.set_size(0);
    header.set_link_name(target).map_err(|error| {
        ComposeError::ExecutionFailed(format!(
            "failed to encode symlink target '{}' for '{}': {error}",
            target.display(),
            entry.rel_path
        ))
    })?;
    header.set_cksum();
    tar.append_data(&mut header, &entry.rel_path, io::empty())
        .map_err(|error| {
            ComposeError::ExecutionFailed(format!(
                "failed to append symlink '{}' to tar stream: {error}",
                entry.rel_path
            ))
        })
}

fn materialize_rootfs_directory(manifest: &RootManifest, output_path: &Path) -> CResult<()> {
    fs::create_dir_all(output_path).map_err(|error| {
        ComposeError::FsFailed(format!(
            "failed to create rootfs directory '{}': {error}",
            output_path.display()
        ))
    })?;

    for entry in &manifest.entries {
        if entry.rel_path.is_empty() {
            continue;
        }

        let destination = output_path.join(&entry.rel_path);
        match &entry.kind {
            EntryKind::Directory => {
                fs::create_dir_all(&destination).map_err(|error| {
                    ComposeError::FsFailed(format!(
                        "failed to create rootfs directory '{}': {error}",
                        destination.display()
                    ))
                })?;
            }
            EntryKind::File { source_path, .. } => {
                ensure_parent_dir(&destination)?;
                materialize_regular_file_with_linker(source_path, &destination, |src, dst| {
                    fs::hard_link(src, dst)
                })?;
            }
            EntryKind::Symlink { target } => {
                ensure_parent_dir(&destination)?;
                create_symlink(target, &destination)?;
            }
        }
    }

    Ok(())
}

fn materialize_regular_file_with_linker<F>(
    source_path: &Path,
    destination: &Path,
    hard_link: F,
) -> CResult<()>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    match hard_link(source_path, destination) {
        Ok(()) => Ok(()),
        Err(error) if can_copy_after_hardlink_error(&error) => {
            fs::copy(source_path, destination).map(|_| ()).map_err(|copy_error| {
                ComposeError::FsFailed(format!(
                    "failed to copy rootfs file '{}' to '{}' after hardlink failed ({error}): {copy_error}",
                    source_path.display(),
                    destination.display()
                ))
            })
        }
        Err(error) => Err(ComposeError::FsFailed(format!(
            "failed to hardlink rootfs file '{}' to '{}': {error}",
            source_path.display(),
            destination.display()
        ))),
    }
}

fn can_copy_after_hardlink_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::CrossesDevices
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
            | io::ErrorKind::TooManyLinks
    )
}

fn ensure_parent_dir(path: &Path) -> CResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            ComposeError::FsFailed(format!(
                "failed to create parent directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, path: &Path) -> CResult<()> {
    std::os::unix::fs::symlink(target, path).map_err(|error| {
        ComposeError::FsFailed(format!(
            "failed to create rootfs symlink '{}' -> '{}': {error}",
            path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _path: &Path) -> CResult<()> {
    Err(ComposeError::FsFailed(
        "Rootfs symlink materialization is only supported on unix platforms".to_string(),
    ))
}

fn read_pipe_to_string(pipe: impl io::Read) -> io::Result<String> {
    let mut reader = io::BufReader::new(pipe);
    let mut out = String::new();
    io::Read::read_to_string(&mut reader, &mut out)?;
    Ok(out)
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> u32 {
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &fs::Metadata) -> u32 {
    0o644
}

fn map_error(error: ComposeError) -> BuilderError {
    BuilderError::ExecutionFailed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbuild_core::BuildContext;
    use serde_json::{Value, json};
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use tempfile::tempdir;

    fn test_context(base: &Path) -> BuildContext {
        let state_dir = base.join("state");
        let temp_dir = base.join("temp");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();
        BuildContext::with_noop_logger(state_dir, temp_dir)
    }

    fn install_meta_with_overrides() -> Map<String, Value> {
        Map::from_iter([(
            "install".to_string(),
            json!({"rules":[
                {"path":"**","attrs":{"uid":0,"gid":0,"directory_mode":493,"regular_file_mode":420,"executable_file_mode":493,"symlink_mode":511}},
                {"path":"var/log/**","attrs":{"uid":100,"gid":200}}
            ]}),
        )])
    }

    fn builder_input(path: PathBuf, meta: Map<String, Value>) -> BuilderInputObject {
        BuilderInputObject {
            object_path: path,
            meta,
        }
    }

    #[test]
    fn install_rules_require_full_coverage_and_last_match_wins() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir_all(root.join("var/log")).unwrap();
        fs::write(root.join("var/log/app.log"), b"log\n").unwrap();

        let install: InstallMeta = serde_json::from_value(json!({
            "rules": [
                {"path": "**", "attrs": {"uid": 0, "gid": 0, "directory_mode": 493, "regular_file_mode": 420, "executable_file_mode": 493, "symlink_mode": 511}},
                {"path": "var/log/**", "attrs": {"uid": 100, "gid": 200}}
            ]
        }))
        .unwrap();
        let rules = compile_install_rules(&install.rules).unwrap();
        let entries = scan_install_tree(&root, &rules).unwrap();
        let log_entry = entries
            .iter()
            .find(|entry| entry.rel_path == "var/log/app.log")
            .unwrap();
        assert_eq!((log_entry.uid, log_entry.gid), (100, 200));

        let uncovered: InstallMeta = serde_json::from_value(json!({
            "rules": [
                {"path": "var/log/*.txt", "attrs": {"uid": 0, "gid": 0}}
            ]
        }))
        .unwrap();
        let rules = compile_install_rules(&uncovered.rules).unwrap();
        let error = scan_install_tree(&root, &rules).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is not covered by any install rule")
        );
    }

    #[test]
    fn scan_uses_executable_class_and_full_final_modes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir_all(root.join("tmp")).unwrap();
        fs::write(root.join("tmp").join("script.sh"), b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(root.join("tmp").join("script.sh"))
                .unwrap()
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(root.join("tmp").join("script.sh"), perms).unwrap();
        }

        let install: InstallMeta = serde_json::from_value(json!({
            "rules": [{
                "path": "**",
                "attrs": {
                    "uid": 0,
                    "gid": 0,
                    "directory_mode": 0o1777,
                    "regular_file_mode": 0o644,
                    "executable_file_mode": 0o4755,
                    "symlink_mode": 0o777
                }
            }]
        }))
        .unwrap();
        let rules = compile_install_rules(&install.rules).unwrap();
        let entries = scan_install_tree(&root, &rules).unwrap();

        let dir_entry = entries
            .iter()
            .find(|entry| entry.rel_path == "tmp")
            .unwrap();
        assert_eq!(dir_entry.mode, 0o1777);

        let file_entry = entries
            .iter()
            .find(|entry| entry.rel_path == "tmp/script.sh")
            .unwrap();
        assert_eq!(file_entry.mode, 0o4755);
    }

    #[test]
    fn merge_rejects_conflicting_non_directory_paths() {
        let entries = vec![
            ContributionEntry {
                rel_path: "bin/tool".to_string(),
                kind: EntryKind::File {
                    source_path: PathBuf::from("/tmp/a"),
                    executable: true,
                },
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
            ContributionEntry {
                rel_path: "bin/tool".to_string(),
                kind: EntryKind::File {
                    source_path: PathBuf::from("/tmp/b"),
                    executable: true,
                },
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
        ];
        let error = merge_contributions(entries).unwrap_err();
        assert!(error.to_string().contains("path conflict"));
    }

    #[test]
    fn merge_allows_matching_directory_overlap_only() {
        let entries = vec![
            ContributionEntry {
                rel_path: "usr".to_string(),
                kind: EntryKind::Directory,
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
            ContributionEntry {
                rel_path: "usr".to_string(),
                kind: EntryKind::Directory,
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
        ];
        let manifest = merge_contributions(entries).unwrap();
        assert!(manifest.entries.iter().any(|entry| entry.rel_path == "usr"));

        let mismatch = vec![
            ContributionEntry {
                rel_path: "usr".to_string(),
                kind: EntryKind::Directory,
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
            ContributionEntry {
                rel_path: "usr".to_string(),
                kind: EntryKind::Directory,
                mode: 0o700,
                uid: 0,
                gid: 0,
            },
        ];
        assert!(merge_contributions(mismatch).is_err());
    }

    #[test]
    fn merge_rejects_file_directory_and_symlink_conflicts() {
        let file_dir = vec![
            ContributionEntry {
                rel_path: "bin".to_string(),
                kind: EntryKind::File {
                    source_path: PathBuf::from("/tmp/file"),
                    executable: false,
                },
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
            ContributionEntry {
                rel_path: "bin".to_string(),
                kind: EntryKind::Directory,
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
        ];
        assert!(merge_contributions(file_dir).is_err());

        let symlinks = vec![
            ContributionEntry {
                rel_path: "lib".to_string(),
                kind: EntryKind::Symlink {
                    target: PathBuf::from("usr/lib"),
                },
                mode: 0o777,
                uid: 0,
                gid: 0,
            },
            ContributionEntry {
                rel_path: "lib".to_string(),
                kind: EntryKind::Symlink {
                    target: PathBuf::from("lib64"),
                },
                mode: 0o777,
                uid: 0,
                gid: 0,
            },
        ];
        assert!(merge_contributions(symlinks).is_err());
    }

    #[test]
    fn rootfs_builder_rejects_empty_inputs() {
        let temp = tempdir().unwrap();
        let mut cx = test_context(temp.path());
        let error = RootfsBuilder
            .build_typed(RootfsConfig {}, BuilderInputs::empty(), &mut cx)
            .unwrap_err();
        assert!(error.to_string().contains("at least one directory input"));
    }

    #[test]
    fn rootfs_builder_rejects_non_directory_input() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("payload");
        fs::write(&file, b"payload\n").unwrap();
        let mut inputs = BuilderInputs::empty();
        inputs.insert("in000", builder_input(file, install_meta_with_overrides()));

        let mut cx = test_context(temp.path());
        let error = RootfsBuilder
            .build_typed(RootfsConfig {}, inputs, &mut cx)
            .unwrap_err();
        assert!(error.to_string().contains("must resolve to a directory"));
    }

    #[test]
    fn rootfs_builder_rejects_missing_install_metadata() {
        let temp = tempdir().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("file"), b"payload\n").unwrap();
        let mut inputs = BuilderInputs::empty();
        inputs.insert("in000", builder_input(tree, Map::new()));

        let mut cx = test_context(temp.path());
        let error = RootfsBuilder
            .build_typed(RootfsConfig {}, inputs, &mut cx)
            .unwrap_err();
        assert!(error.to_string().contains("missing meta.install"));
    }

    #[cfg(unix)]
    #[test]
    fn rootfs_builder_materializes_directory_files_and_symlinks() {
        let temp = tempdir().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir_all(tree.join("bin")).unwrap();
        fs::write(tree.join("bin/tool"), b"hello\n").unwrap();
        let mut perms = fs::metadata(tree.join("bin/tool")).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(tree.join("bin/tool"), perms).unwrap();
        symlink("tool", tree.join("bin/tool-link")).unwrap();

        let mut inputs = BuilderInputs::empty();
        inputs.insert(
            "pkg",
            builder_input(tree.clone(), install_meta_with_overrides()),
        );

        let mut cx = test_context(temp.path());
        let result = RootfsBuilder
            .build_typed(RootfsConfig {}, inputs, &mut cx)
            .unwrap();

        assert_eq!(result.meta, Map::new());
        assert!(result.staged_path.is_dir());
        assert!(result.staged_path.join("bin").is_dir());
        assert_eq!(
            fs::read_to_string(result.staged_path.join("bin/tool")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            fs::read_link(result.staged_path.join("bin/tool-link")).unwrap(),
            Path::new("tool")
        );

        let source_meta = fs::metadata(tree.join("bin/tool")).unwrap();
        let output_meta = fs::metadata(result.staged_path.join("bin/tool")).unwrap();
        assert_eq!(source_meta.dev(), output_meta.dev());
        assert_eq!(source_meta.ino(), output_meta.ino());
    }

    #[test]
    fn rootfs_file_materialization_copies_when_hardlink_fallback_is_valid() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::write(&source, b"copy fallback\n").unwrap();

        materialize_regular_file_with_linker(&source, &destination, |_, _| {
            Err(io::Error::from(io::ErrorKind::CrossesDevices))
        })
        .unwrap();

        assert_eq!(fs::read_to_string(destination).unwrap(), "copy fallback\n");
    }

    #[test]
    fn ext4_builder_rejects_missing_install_metadata() {
        let temp = tempdir().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("file"), b"payload\n").unwrap();
        let mut inputs = BuilderInputs::empty();
        inputs.insert("in000", builder_input(tree, Map::new()));

        let mut cx = test_context(temp.path());
        let error = Ext4RootfsBuilder
            .build_typed(
                Ext4RootfsConfig {
                    size_mib: 16,
                    label: None,
                },
                inputs,
                &mut cx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("missing meta.install"));
    }

    #[test]
    fn ext4_builder_builds_deterministic_image_with_expected_metadata() {
        let temp = tempdir().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir_all(tree.join("bin")).unwrap();
        fs::create_dir_all(tree.join("var/log")).unwrap();
        fs::write(tree.join("bin/tool"), b"hello\n").unwrap();
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(tree.join("bin/tool")).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(tree.join("bin/tool"), perms).unwrap();
            symlink("tool", tree.join("bin/tool-link")).unwrap();
        }

        let build_once = |suffix: &str| {
            let mut inputs = BuilderInputs::empty();
            inputs.insert(
                "in000",
                builder_input(tree.clone(), install_meta_with_overrides()),
            );
            let ctx_root = temp.path().join(format!("ctx-{suffix}"));
            fs::create_dir_all(&ctx_root).unwrap();
            let mut cx = test_context(&ctx_root);
            Ext4RootfsBuilder
                .build_typed(
                    Ext4RootfsConfig {
                        size_mib: 16,
                        label: Some("rootfs".to_string()),
                    },
                    inputs,
                    &mut cx,
                )
                .unwrap()
        };

        let first = build_once("one");
        let second = build_once("two");
        let first_bytes = fs::read(&first.staged_path).unwrap();
        let second_bytes = fs::read(&second.staged_path).unwrap();
        assert_eq!(first.meta, Map::new());
        assert_images_equal_with_diagnostics(
            &first.staged_path,
            &first_bytes,
            &second.staged_path,
            &second_bytes,
        );

        let listing = debugfs_output(&first.staged_path, "ls -l /bin");
        assert!(listing.contains("tool"));
        assert!(listing.contains("tool-link"));

        let stat_file = debugfs_output(&first.staged_path, "stat /bin/tool");
        assert!(stat_file.contains("Mode:  0755"));
        assert!(stat_file.contains("User:     0"));
        assert!(stat_file.contains("Group:     0"));

        let stat_log_dir = debugfs_output(&first.staged_path, "stat /var/log");
        assert!(stat_log_dir.contains("User:   100"));
        assert!(stat_log_dir.contains("Group:   200"));

        let stat_link = debugfs_output(&first.staged_path, "stat /bin/tool-link");
        assert!(stat_link.contains("Type: symlink"));
        assert!(stat_link.contains("Fast link dest: \"tool\""));

        let contents = debugfs_output(&first.staged_path, "cat /bin/tool");
        assert_eq!(contents, "hello\n");
    }

    fn debugfs_output(image: &Path, command: &str) -> String {
        let output = Command::new("debugfs")
            .arg("-R")
            .arg(command)
            .arg(image)
            .output()
            .unwrap();
        assert!(output.status.success(), "debugfs failed: {:?}", output);
        String::from_utf8(output.stdout).unwrap()
    }

    fn assert_images_equal_with_diagnostics(
        first_path: &Path,
        first_bytes: &[u8],
        second_path: &Path,
        second_bytes: &[u8],
    ) {
        if first_bytes == second_bytes {
            return;
        }

        let first_diff = first_difference_offset(first_bytes, second_bytes);
        let first_window = hex_window(first_bytes, first_diff);
        let second_window = hex_window(second_bytes, first_diff);
        let first_summary = tune2fs_summary(first_path);
        let second_summary = tune2fs_summary(second_path);

        panic!(
            "ext4 images differ\nfirst: {}\nsecond: {}\nfirst size: {}\nsecond size: {}\nfirst differing offset: {}\nfirst bytes: {}\nsecond bytes: {}\nfirst tune2fs:\n{}\nsecond tune2fs:\n{}",
            first_path.display(),
            second_path.display(),
            first_bytes.len(),
            second_bytes.len(),
            first_diff,
            first_window,
            second_window,
            first_summary,
            second_summary,
        );
    }

    fn first_difference_offset(first: &[u8], second: &[u8]) -> usize {
        let shared = first.len().min(second.len());
        for index in 0..shared {
            if first[index] != second[index] {
                return index;
            }
        }
        shared
    }

    fn hex_window(bytes: &[u8], offset: usize) -> String {
        let start = offset.saturating_sub(16);
        let end = bytes.len().min(offset.saturating_add(16));
        let mut rendered = String::new();
        for (index, byte) in bytes[start..end].iter().enumerate() {
            if index > 0 {
                rendered.push(' ');
            }
            rendered.push_str(&format!("{:02x}", byte));
        }
        format!("offset {}..{} [{}]", start, end, rendered)
    }

    fn tune2fs_summary(image: &Path) -> String {
        let output = match Command::new("tune2fs").arg("-l").arg(image).output() {
            Ok(output) => output,
            Err(error) => {
                return format!("tune2fs unavailable for '{}': {}", image.display(), error);
            }
        };

        if !output.status.success() {
            return format!(
                "tune2fs failed for '{}': {}",
                image.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let wanted = [
            "Filesystem UUID:",
            "Filesystem volume name:",
            "Filesystem features:",
            "Directory Hash Seed:",
            "Filesystem created:",
            "Last mount time:",
            "Last write time:",
            "Last checked:",
        ];

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut selected = stdout
            .lines()
            .filter(|line| wanted.iter().any(|prefix| line.starts_with(prefix)))
            .collect::<Vec<_>>();

        if selected.is_empty() {
            selected.push("<no selected tune2fs lines>");
        }

        selected.join("\n")
    }

    #[cfg(unix)]
    #[test]
    fn scan_rejects_special_files() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir_all(&root).unwrap();
        let fifo_path = root.join("pipe");
        let c_fifo = std::ffi::CString::new(fifo_path.to_string_lossy().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o644) };
        assert_eq!(rc, 0);

        let rules = compile_install_rules(
            &serde_json::from_value::<InstallMeta>(json!({
                "rules": [{
                    "path": "**",
                    "attrs": {
                        "uid": 0,
                        "gid": 0,
                        "directory_mode": 493,
                        "regular_file_mode": 420,
                        "executable_file_mode": 493,
                        "symlink_mode": 511
                    }
                }]
            }))
            .unwrap()
            .rules,
        )
        .unwrap();
        let error = scan_install_tree(&root, &rules).unwrap_err();
        assert!(error.to_string().contains("unsupported file type"));
    }
}
