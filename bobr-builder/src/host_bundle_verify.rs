//! Structural validation for staged HostBundle directory objects.

use bobr_bundle_launcher::{
    BundleConfig, ElfLinkage, ExecutableFormat, PlatformArch, ResolvedTool,
    ToolVisibility, inspect_elf_for_arch, inspect_executable_for_arch, locate_bundle_from_launcher,
    resolve_tool,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const LAUNCHER_RELATIVE_PATH: &str = "libexec/bobr-bundle-launcher";
const WRAPPED_BIN: &str = "libexec/wrapped-bin";

/// Structurally validated tools in deterministic configuration order.
#[derive(Debug)]
pub(crate) struct VerifiedStructure {
    pub(crate) tools: Vec<ResolvedTool>,
}

/// Validates the generated layout and every path needed to enter a configured tool.
pub(crate) fn verify_structure(
    bundle_root: &Path,
    expected: &BundleConfig,
) -> Result<VerifiedStructure, String> {
    let launcher = bundle_root.join(LAUNCHER_RELATIVE_PATH);
    let launcher_metadata = fs::symlink_metadata(&launcher).map_err(|error| {
        format!(
            "failed to inspect launcher '{}': {error}",
            launcher.display()
        )
    })?;
    if !launcher_metadata.file_type().is_file()
        || launcher_metadata.permissions().mode() & 0o111 == 0
    {
        return Err(format!(
            "HostBundle launcher '{}' must be an executable regular file",
            launcher.display()
        ));
    }
    let launcher_elf = inspect_elf_for_arch(&launcher, PlatformArch::X86_64).map_err(|error| {
        format!(
            "invalid HostBundle launcher '{}': {error}",
            launcher.display()
        )
    })?;
    if !matches!(launcher_elf.linkage(), ElfLinkage::Static) {
        return Err(format!(
            "HostBundle launcher '{}' must not contain PT_INTERP",
            launcher.display()
        ));
    }

    let on_disk = BundleConfig::parse(
        &fs::read_to_string(bundle_root.join("bundle.toml"))
            .map_err(|error| format!("failed to read generated bundle.toml: {error}"))?,
    )
    .map_err(|error| format!("invalid generated bundle.toml: {error}"))?;
    if &on_disk != expected {
        return Err("generated bundle.toml differs from the validated runtime config".to_string());
    }

    let location = locate_bundle_from_launcher(&launcher)
        .map_err(|error| format!("invalid HostBundle launcher layout: {error}"))?;
    let mut tools = Vec::with_capacity(expected.tools.len());
    for (name, config) in &expected.tools {
        let resolved = resolve_tool(&location, expected, name)
            .map_err(|error| format!("invalid HostBundle tool '{name}': {error}"))?;
        inspect_executable_for_arch(resolved.target(), expected.platform.arch)
            .map_err(|error| format!("invalid HostBundle tool '{name}': {error}"))?;
        verify_wrapper(
            &bundle_root.join(WRAPPED_BIN).join(name),
            Path::new("../bobr-bundle-launcher"),
            "internal",
            name,
        )?;
        let public_wrapper = bundle_root.join("bin").join(name);
        match config.visibility {
            ToolVisibility::Public => verify_wrapper(
                &public_wrapper,
                Path::new("../libexec/bobr-bundle-launcher"),
                "public",
                name,
            )?,
            ToolVisibility::Internal => {
                if fs::symlink_metadata(&public_wrapper).is_ok() {
                    return Err(format!(
                        "internal HostBundle tool '{name}' unexpectedly has a public wrapper"
                    ));
                }
            }
        }
        tools.push(resolved);
    }

    for (index, directory) in expected.loader.library_dirs.iter().enumerate() {
        let path = resolve_inside_bundle(
            bundle_root,
            directory,
            &format!("loader.library_dirs[{index}]"),
        )?;
        if !path.is_dir() {
            return Err(format!(
                "loader.library_dirs[{index}] '{}' is not a directory",
                path.display()
            ));
        }
    }
    verify_environment_paths(bundle_root, "environment", &expected.environment)?;
    for (name, tool) in &expected.tools {
        verify_environment_paths(
            bundle_root,
            &format!("tools.{name}.environment"),
            &tool.environment,
        )?;
    }

    Ok(VerifiedStructure { tools })
}

fn verify_wrapper(
    path: &Path,
    expected_target: &Path,
    kind: &str,
    tool: &str,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "missing {kind} wrapper for HostBundle tool '{tool}' at '{}': {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_symlink() {
        return Err(format!(
            "{kind} wrapper for HostBundle tool '{tool}' is not a symlink"
        ));
    }
    let actual = fs::read_link(path)
        .map_err(|error| format!("failed to read wrapper '{}': {error}", path.display()))?;
    if actual != expected_target {
        return Err(format!(
            "{kind} wrapper for HostBundle tool '{tool}' points to '{}' instead of '{}'",
            actual.display(),
            expected_target.display()
        ));
    }
    Ok(())
}

fn verify_environment_paths(
    bundle_root: &Path,
    scope: &str,
    rules: &std::collections::BTreeMap<String, bobr_bundle_launcher::EnvironmentRule>,
) -> Result<(), String> {
    for (variable, rule) in rules {
        for (index, path) in rule.paths.iter().enumerate() {
            resolve_inside_bundle(
                bundle_root,
                path,
                &format!("{scope}.{variable}.paths[{index}]"),
            )?;
        }
    }
    Ok(())
}

fn resolve_inside_bundle(
    bundle_root: &Path,
    relative: &str,
    field: &str,
) -> Result<PathBuf, String> {
    let canonical_root = fs::canonicalize(bundle_root)
        .map_err(|error| format!("failed to resolve HostBundle root: {error}"))?;
    let candidate = canonical_root.join(relative);
    let resolved = fs::canonicalize(&candidate).map_err(|error| {
        format!(
            "failed to resolve {field} '{}': {error}",
            candidate.display()
        )
    })?;
    if !resolved.starts_with(&canonical_root) {
        return Err(format!(
            "{field} resolves to '{}' outside HostBundle root '{}'",
            resolved.display(),
            canonical_root.display()
        ));
    }
    Ok(resolved)
}

/// Returns whether a structurally inspected tool is a script.
pub(crate) fn is_script(tool: &ResolvedTool) -> Result<bool, String> {
    inspect_executable_for_arch(tool.target(), PlatformArch::X86_64)
        .map(|format| matches!(format, ExecutableFormat::Script(_)))
        .map_err(|error| format!("failed to inspect '{}': {error}", tool.target().display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_bundle_launcher::{
        EnvironmentOperation, EnvironmentRule, HostPolicy, LoaderConfig, LoaderKind,
        PlatformConfig, PlatformOs, ToolConfig,
    };
    use std::collections::BTreeMap;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::tempdir;

    fn static_elf(path: &Path) {
        let mut bytes = vec![0_u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, BundleConfig) {
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join("root/usr/bin")).unwrap();
        fs::create_dir_all(temp.path().join("root/usr/lib")).unwrap();
        fs::create_dir_all(temp.path().join("libexec/wrapped-bin")).unwrap();
        fs::create_dir(temp.path().join("bin")).unwrap();
        static_elf(&temp.path().join(LAUNCHER_RELATIVE_PATH));
        static_elf(&temp.path().join("root/usr/bin/demo"));
        symlink(
            "../bobr-bundle-launcher",
            temp.path().join("libexec/wrapped-bin/demo"),
        )
        .unwrap();
        symlink(
            "../libexec/bobr-bundle-launcher",
            temp.path().join("bin/demo"),
        )
        .unwrap();
        let config = BundleConfig::new_v1(
            "root",
            HostPolicy::Strict,
            PlatformConfig {
                os: PlatformOs::Linux,
                arch: PlatformArch::X86_64,
                min_kernel: "4.19".to_string(),
            },
            LoaderConfig {
                kind: LoaderKind::Glibc,
                library_dirs: vec!["root/usr/lib".to_string()],
                inhibit_cache: true,
            },
            BTreeMap::from([(
                "PATH".to_string(),
                EnvironmentRule {
                    operation: EnvironmentOperation::Prepend,
                    paths: vec!["libexec/wrapped-bin".to_string()],
                    values: Vec::new(),
                    inherit: true,
                    host_default: Vec::new(),
                },
            )]),
            BTreeMap::from([(
                "demo".to_string(),
                ToolConfig {
                    path: "root/usr/bin/demo".to_string(),
                    argv0: "demo".to_string(),
                    visibility: ToolVisibility::Public,
                    environment: BTreeMap::new(),
                },
            )]),
        )
        .unwrap();
        fs::write(temp.path().join("bundle.toml"), config.to_toml().unwrap()).unwrap();
        (temp, config)
    }

    #[test]
    fn accepts_exact_generated_layout() {
        let (temp, config) = fixture();
        let verified = verify_structure(temp.path(), &config).unwrap();
        assert_eq!(verified.tools.len(), 1);
        assert!(!is_script(&verified.tools[0]).unwrap());
    }

    #[test]
    fn rejects_escaping_tool_and_wrong_wrapper() {
        let (temp, config) = fixture();
        fs::remove_file(temp.path().join("root/usr/bin/demo")).unwrap();
        symlink("/bin/sh", temp.path().join("root/usr/bin/demo")).unwrap();
        let error = verify_structure(temp.path(), &config).unwrap_err();
        assert!(error.contains("outside payload root"), "{error}");

        let (temp, config) = fixture();
        fs::remove_file(temp.path().join("bin/demo")).unwrap();
        symlink("../wrong", temp.path().join("bin/demo")).unwrap();
        let error = verify_structure(temp.path(), &config).unwrap_err();
        assert!(error.contains("instead of"), "{error}");
    }
}
