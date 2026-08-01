//! Structural validation for staged HostBundle directory objects.

use bobr_bundle_launcher::{
    BundleConfig, ElfLinkage, ExecutableFormat, MAX_SCRIPT_DEPTH, PlatformArch, ResolvedTool,
    Shebang, ToolVisibility, inspect_elf_for_arch, inspect_executable_for_arch,
    locate_bundle_from_launcher, resolve_tool,
};
use goblin::elf::Elf;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const LAUNCHER_RELATIVE_PATH: &str = "libexec/bobr-bundle-launcher";
const WRAPPED_BIN: &str = "libexec/wrapped-bin";
const ENV_INTERPRETER: &str = "/usr/bin/env";
const MAX_STARTUP_NODES: usize = 4096;

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
    let launcher_elf =
        inspect_elf_for_arch(&launcher, expected.platform.arch).map_err(|error| {
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
    rules: &BTreeMap<String, bobr_bundle_launcher::EnvironmentRule>,
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

/// Proves the ELF/script startup closure using only staged HostBundle paths.
pub(crate) fn verify_startup_closure(
    bundle_root: &Path,
    config: &BundleConfig,
    structure: &VerifiedStructure,
) -> Result<(), String> {
    let payload_root = fs::canonicalize(bundle_root.join(&config.payload_root))
        .map_err(|error| format!("failed to resolve payload root: {error}"))?;
    let mut library_dirs = Vec::with_capacity(config.loader.library_dirs.len());
    for (index, relative) in config.loader.library_dirs.iter().enumerate() {
        let directory = resolve_inside_bundle(
            bundle_root,
            relative,
            &format!("loader.library_dirs[{index}]"),
        )?;
        if !directory.starts_with(&payload_root) {
            return Err(format!(
                "loader.library_dirs[{index}] resolves outside payload root"
            ));
        }
        library_dirs.push(directory);
    }

    let launcher = bundle_root.join(LAUNCHER_RELATIVE_PATH);
    let launcher_info = parse_elf(&launcher, config.platform.arch)?;
    if launcher_info.interpreter.is_some() || !launcher_info.needed.is_empty() {
        return Err(format!(
            "HostBundle launcher '{}' must have neither PT_INTERP nor DT_NEEDED",
            launcher.display()
        ));
    }

    let managed_tools = structure
        .tools
        .iter()
        .map(|tool| (tool.name().to_string(), tool.target().to_path_buf()))
        .collect();
    let mut verifier = StartupVerifier {
        payload_root,
        library_dirs,
        managed_tools,
        platform_arch: config.platform.arch,
        active_executables: BTreeSet::new(),
        active_libraries: BTreeSet::new(),
        verified_executables: BTreeSet::new(),
        verified_libraries: BTreeSet::new(),
        visited_states: 0,
    };
    for tool in &structure.tools {
        verifier.audit_executable(tool.target(), &[], tool.name(), 0)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExecutableState {
    path: PathBuf,
    inherited_rpaths: Vec<PathBuf>,
    script_depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LibraryState {
    path: PathBuf,
    inherited_rpaths: Vec<PathBuf>,
}

struct StartupVerifier {
    payload_root: PathBuf,
    library_dirs: Vec<PathBuf>,
    managed_tools: BTreeMap<String, PathBuf>,
    platform_arch: PlatformArch,
    active_executables: BTreeSet<PathBuf>,
    active_libraries: BTreeSet<PathBuf>,
    verified_executables: BTreeSet<ExecutableState>,
    verified_libraries: BTreeSet<LibraryState>,
    visited_states: usize,
}

impl StartupVerifier {
    fn audit_executable(
        &mut self,
        path: &Path,
        inherited_rpaths: &[PathBuf],
        context: &str,
        script_depth: usize,
    ) -> Result<(), String> {
        let canonical = self.resolve_existing_executable(path, context)?;
        let state = ExecutableState {
            path: canonical.clone(),
            inherited_rpaths: inherited_rpaths.to_vec(),
            script_depth,
        };
        if self.verified_executables.contains(&state) {
            return Ok(());
        }
        if !self.active_executables.insert(canonical.clone()) {
            return Err(format!(
                "script/interpreter startup cycle reaches '{}'",
                canonical.display()
            ));
        }
        self.bump_state(&canonical)?;
        let result = match inspect_executable_for_arch(&canonical, self.platform_arch)
            .map_err(|error| format!("invalid executable '{}': {error}", canonical.display()))?
        {
            ExecutableFormat::Elf(_) => self.audit_elf_entry(
                &canonical,
                inherited_rpaths,
                &format!("executable '{context}'"),
            ),
            ExecutableFormat::Script(shebang) => {
                if script_depth >= MAX_SCRIPT_DEPTH {
                    Err(format!(
                        "shebang recursion limit ({MAX_SCRIPT_DEPTH}) exceeded at '{}'",
                        canonical.display()
                    ))
                } else {
                    self.audit_script(
                        &canonical,
                        &shebang,
                        inherited_rpaths,
                        context,
                        script_depth,
                    )
                }
            }
        };
        self.active_executables.remove(&canonical);
        if result.is_ok() {
            self.verified_executables.insert(state);
        }
        result
    }

    fn audit_script(
        &mut self,
        script: &Path,
        shebang: &Shebang,
        inherited_rpaths: &[PathBuf],
        context: &str,
        script_depth: usize,
    ) -> Result<(), String> {
        let interpreter = self.resolve_logical_payload_path(
            shebang.interpreter(),
            &format!("shebang interpreter of '{}'", script.display()),
        )?;
        if shebang.interpreter() == Path::new(ENV_INTERPRETER) {
            let argument = shebang.argument().ok_or_else(|| {
                format!(
                    "script '{}' uses {ENV_INTERPRETER} without a managed command",
                    script.display()
                )
            })?;
            let bytes = argument.as_bytes();
            if bytes.is_empty()
                || bytes.starts_with(b"-")
                || bytes.contains(&b'/')
                || bytes.iter().any(u8::is_ascii_whitespace)
            {
                return Err(format!(
                    "script '{}' uses unsupported {ENV_INTERPRETER} argument '{}'; \
                     HostBundle v1 accepts exactly one managed command name",
                    script.display(),
                    argument.to_string_lossy()
                ));
            }
            let command = std::str::from_utf8(bytes).map_err(|_| {
                format!(
                    "script '{}' uses a non-UTF-8 {ENV_INTERPRETER} command",
                    script.display()
                )
            })?;
            let command_path = self.managed_tools.get(command).cloned().ok_or_else(|| {
                format!(
                    "script '{}' asks {ENV_INTERPRETER} for undeclared command '{command}'",
                    script.display()
                )
            })?;
            self.audit_executable(
                &interpreter,
                inherited_rpaths,
                &format!("{context} shebang env"),
                script_depth + 1,
            )?;
            self.audit_executable(
                &command_path,
                &[],
                &format!("{context} env command '{command}'"),
                0,
            )
        } else {
            self.audit_executable(
                &interpreter,
                inherited_rpaths,
                &format!("{context} shebang interpreter"),
                script_depth + 1,
            )
        }
    }

    fn audit_elf_entry(
        &mut self,
        path: &Path,
        inherited_rpaths: &[PathBuf],
        context: &str,
    ) -> Result<(), String> {
        let info = parse_elf(path, self.platform_arch)?;
        match &info.interpreter {
            Some(interpreter) => {
                let loader = self.resolve_logical_payload_path(
                    Path::new(interpreter),
                    &format!("PT_INTERP of '{}'", path.display()),
                )?;
                let loader_info = parse_elf(&loader, self.platform_arch)?;
                if loader_info.interpreter.is_some() || !loader_info.needed.is_empty() {
                    return Err(format!(
                        "bundled loader '{}' must have neither PT_INTERP nor DT_NEEDED",
                        loader.display()
                    ));
                }
                if self.library_dirs.is_empty() {
                    return Err(format!(
                        "{context} '{}' is dynamically linked, but loader.library_dirs is empty",
                        path.display()
                    ));
                }
            }
            None if !info.needed.is_empty() => {
                return Err(format!(
                    "{context} '{}' has DT_NEEDED but no PT_INTERP",
                    path.display()
                ));
            }
            None => return Ok(()),
        }
        self.audit_dependencies(path, &info, inherited_rpaths)
    }

    fn audit_library(&mut self, path: &Path, inherited_rpaths: &[PathBuf]) -> Result<(), String> {
        let canonical = self.resolve_existing_file(path, "DT_NEEDED library")?;
        let state = LibraryState {
            path: canonical.clone(),
            inherited_rpaths: inherited_rpaths.to_vec(),
        };
        if self.verified_libraries.contains(&state) {
            return Ok(());
        }
        if !self.active_libraries.insert(canonical.clone()) {
            return Ok(());
        }
        self.bump_state(&canonical)?;
        let info = parse_elf(&canonical, self.platform_arch)?;
        // PT_INTERP is only consulted when the kernel executes an ELF. It is
        // ignored when the same ET_DYN object is loaded through DT_NEEDED;
        // glibc's libc.so.6 intentionally has PT_INTERP so it can also be run
        // as a program.
        let result = self.audit_dependencies(&canonical, &info, inherited_rpaths);
        self.active_libraries.remove(&canonical);
        if result.is_ok() {
            self.verified_libraries.insert(state);
        }
        result
    }

    fn audit_dependencies(
        &mut self,
        object: &Path,
        info: &ElfInfo,
        inherited_rpaths: &[PathBuf],
    ) -> Result<(), String> {
        let origin = object
            .parent()
            .ok_or_else(|| format!("ELF path '{}' has no parent", object.display()))?;
        let declared_rpaths =
            expand_dynamic_paths(&info.rpaths, origin, &self.payload_root, "DT_RPATH", object)?;
        let local_runpaths = expand_dynamic_paths(
            &info.runpaths,
            origin,
            &self.payload_root,
            "DT_RUNPATH",
            object,
        )?;
        let local_rpaths = if info.runpaths.is_empty() {
            declared_rpaths
        } else {
            // glibc ignores DT_RPATH when the same object has DT_RUNPATH. The
            // declaration was still expanded above so forbidden entries fail.
            Vec::new()
        };

        let mut rpath_search = local_rpaths.clone();
        rpath_search.extend_from_slice(inherited_rpaths);
        deduplicate_paths(&mut rpath_search);
        let mut search = rpath_search.clone();
        search.extend(self.library_dirs.iter().cloned());
        search.extend(local_runpaths);
        deduplicate_paths(&mut search);

        let mut child_inherited = local_rpaths;
        child_inherited.extend_from_slice(inherited_rpaths);
        deduplicate_paths(&mut child_inherited);

        for needed in &info.needed {
            if needed.is_empty() || needed.as_bytes().contains(&b'/') {
                return Err(format!(
                    "ELF '{}' contains forbidden DT_NEEDED '{needed}'",
                    object.display()
                ));
            }
            let dependency = self.resolve_needed(object, needed, &search)?;
            let dependency_info = parse_elf(&dependency, self.platform_arch)?;
            if let Some(soname) = &dependency_info.soname
                && soname != needed
            {
                return Err(format!(
                    "DT_NEEDED '{needed}' from '{}' resolves to '{}' with SONAME '{soname}'",
                    object.display(),
                    dependency.display()
                ));
            }
            verify_symbol_versions(
                object,
                needed,
                &dependency,
                info.required_versions.get(needed),
                &dependency_info.provided_versions,
            )?;
            self.audit_library(&dependency, &child_inherited)?;
        }
        Ok(())
    }

    fn resolve_needed(
        &self,
        object: &Path,
        needed: &str,
        search: &[PathBuf],
    ) -> Result<PathBuf, String> {
        for directory in search {
            let candidate = directory.join(needed);
            match fs::symlink_metadata(&candidate) {
                Ok(_) => {
                    // A dangling entry shadows all later search directories in
                    // the loader's ordered lookup. Treat it as an error rather
                    // than allowing a later library to hide a bundle defect.
                    let resolved = fs::canonicalize(&candidate).map_err(|error| {
                        format!(
                            "failed to resolve DT_NEEDED '{needed}' candidate '{}': {error}",
                            candidate.display()
                        )
                    })?;
                    if !resolved.starts_with(&self.payload_root) {
                        return Err(format!(
                            "DT_NEEDED '{needed}' from '{}' resolves outside payload to '{}'",
                            object.display(),
                            resolved.display()
                        ));
                    }
                    if !resolved.is_file() {
                        return Err(format!(
                            "DT_NEEDED '{needed}' from '{}' resolves to non-file '{}'",
                            object.display(),
                            resolved.display()
                        ));
                    }
                    return Ok(resolved);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to inspect DT_NEEDED '{needed}' candidate '{}': {error}",
                        candidate.display()
                    ));
                }
            }
        }
        Err(format!(
            "ELF '{}' cannot resolve DT_NEEDED '{needed}' inside HostBundle",
            object.display()
        ))
    }

    fn resolve_logical_payload_path(&self, logical: &Path, field: &str) -> Result<PathBuf, String> {
        if !logical.is_absolute()
            || logical.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::CurDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(format!(
                "{field} '{}' is not a safe absolute payload path",
                logical.display()
            ));
        }
        let relative = logical
            .strip_prefix("/")
            .map_err(|_| format!("{field} '{}' is invalid", logical.display()))?;
        self.resolve_existing_executable(&self.payload_root.join(relative), field)
    }

    fn resolve_existing_executable(&self, path: &Path, field: &str) -> Result<PathBuf, String> {
        let resolved = self.resolve_existing_file(path, field)?;
        let metadata = fs::metadata(&resolved).map_err(|error| {
            format!(
                "failed to inspect {field} '{}': {error}",
                resolved.display()
            )
        })?;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "{field} '{}' is not executable",
                resolved.display()
            ));
        }
        Ok(resolved)
    }

    fn resolve_existing_file(&self, path: &Path, field: &str) -> Result<PathBuf, String> {
        let resolved = fs::canonicalize(path)
            .map_err(|error| format!("failed to resolve {field} '{}': {error}", path.display()))?;
        if !resolved.starts_with(&self.payload_root) {
            return Err(format!(
                "{field} resolves to '{}' outside payload root '{}'",
                resolved.display(),
                self.payload_root.display()
            ));
        }
        if !resolved.is_file() {
            return Err(format!(
                "{field} '{}' is not a regular file",
                resolved.display()
            ));
        }
        Ok(resolved)
    }

    fn bump_state(&mut self, path: &Path) -> Result<(), String> {
        self.visited_states += 1;
        if self.visited_states > MAX_STARTUP_NODES {
            return Err(format!(
                "HostBundle startup graph exceeds {MAX_STARTUP_NODES} unique states near '{}'",
                path.display()
            ));
        }
        Ok(())
    }
}

fn verify_symbol_versions(
    object: &Path,
    needed: &str,
    dependency: &Path,
    required: Option<&BTreeSet<String>>,
    provided: &BTreeSet<String>,
) -> Result<(), String> {
    let Some(required) = required else {
        return Ok(());
    };
    let missing = required.difference(provided).cloned().collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "DT_NEEDED '{needed}' from '{}' resolves to '{}' without required symbol version(s): {}",
        object.display(),
        dependency.display(),
        missing.join(", ")
    ))
}

#[derive(Debug)]
struct ElfInfo {
    interpreter: Option<String>,
    needed: Vec<String>,
    rpaths: Vec<String>,
    runpaths: Vec<String>,
    soname: Option<String>,
    required_versions: BTreeMap<String, BTreeSet<String>>,
    provided_versions: BTreeSet<String>,
}

fn parse_elf(path: &Path, expected_arch: PlatformArch) -> Result<ElfInfo, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read ELF '{}': {error}", path.display()))?;
    let elf = Elf::parse(&bytes)
        .map_err(|error| format!("failed to parse ELF '{}': {error}", path.display()))?;
    let expected_machine = expected_arch.elf_machine();
    if !elf.is_64 || !elf.little_endian || elf.header.e_machine != expected_machine {
        return Err(format!(
            "ELF '{}' does not match HostBundle architecture {expected_arch}",
            path.display(),
        ));
    }

    let mut required_versions = BTreeMap::<String, BTreeSet<String>>::new();
    if let Some(section) = &elf.verneed {
        for need in section.iter() {
            let library = elf.dynstrtab.get_at(need.vn_file).ok_or_else(|| {
                format!(
                    "ELF '{}' has an invalid GNU version-needed library name",
                    path.display()
                )
            })?;
            let versions = required_versions.entry(library.to_string()).or_default();
            for auxiliary in need.iter() {
                let version = elf.dynstrtab.get_at(auxiliary.vna_name).ok_or_else(|| {
                    format!(
                        "ELF '{}' has an invalid GNU version-needed name",
                        path.display()
                    )
                })?;
                versions.insert(version.to_string());
            }
        }
    }

    let mut provided_versions = BTreeSet::new();
    if let Some(section) = &elf.verdef {
        for definition in section.iter() {
            for auxiliary in definition.iter() {
                let version = elf.dynstrtab.get_at(auxiliary.vda_name).ok_or_else(|| {
                    format!(
                        "ELF '{}' has an invalid GNU version-definition name",
                        path.display()
                    )
                })?;
                provided_versions.insert(version.to_string());
            }
        }
    }

    Ok(ElfInfo {
        interpreter: elf.interpreter.map(str::to_string),
        needed: elf
            .libraries
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        rpaths: elf
            .rpaths
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        runpaths: elf
            .runpaths
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        soname: elf.soname.map(str::to_string),
        required_versions,
        provided_versions,
    })
}

fn expand_dynamic_paths(
    values: &[String],
    origin: &Path,
    payload_root: &Path,
    tag: &str,
    object: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    for value in values {
        for entry in value.split(':') {
            let suffix = entry
                .strip_prefix("$ORIGIN")
                .or_else(|| entry.strip_prefix("${ORIGIN}"))
                .ok_or_else(|| {
                    format!(
                        "ELF '{}' has unsupported {tag} entry '{entry}'; \
                         HostBundle v1 accepts only $ORIGIN-relative entries",
                        object.display()
                    )
                })?;
            if !(suffix.is_empty() || suffix.starts_with('/')) || suffix.contains('$') {
                return Err(format!(
                    "ELF '{}' has unsupported {tag} entry '{entry}'",
                    object.display()
                ));
            }
            let directory = origin.join(suffix.trim_start_matches('/'));
            if let Ok(canonical) = fs::canonicalize(&directory) {
                if !canonical.starts_with(payload_root) {
                    return Err(format!(
                        "ELF '{}' {tag} entry '{entry}' resolves outside payload to '{}'",
                        object.display(),
                        canonical.display()
                    ));
                }
                result.push(canonical);
            } else {
                // Preserve a missing $ORIGIN-relative directory in search
                // order. A concrete DT_NEEDED candidate is still resolved and
                // checked for payload containment before it can be accepted.
                result.push(directory);
            }
        }
    }
    Ok(result)
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
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
        let config = BundleConfig::new_v2(
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
                    argument_prefix: Vec::new(),
                    environment: BTreeMap::new(),
                },
            )]),
        )
        .unwrap();
        fs::write(temp.path().join("bundle.toml"), config.to_toml().unwrap()).unwrap();
        (temp, config)
    }

    fn write_config(root: &Path, config: &BundleConfig) {
        fs::write(root.join("bundle.toml"), config.to_toml().unwrap()).unwrap();
    }

    fn script(path: &Path, interpreter: &str) {
        fs::write(path, format!("#!{interpreter}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn accepts_exact_generated_layout() {
        let (temp, config) = fixture();
        let verified = verify_structure(temp.path(), &config).unwrap();
        assert_eq!(verified.tools.len(), 1);
        verify_startup_closure(temp.path(), &config, &verified).unwrap();
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

    #[test]
    fn accepts_simple_managed_env_shebang_and_rejects_env_options() {
        let (temp, mut config) = fixture();
        fs::write(
            temp.path().join("root/usr/bin/demo"),
            b"#!/usr/bin/env helper\n",
        )
        .unwrap();
        fs::set_permissions(
            temp.path().join("root/usr/bin/demo"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        static_elf(&temp.path().join("root/usr/bin/env"));
        static_elf(&temp.path().join("root/usr/bin/helper"));
        for name in ["env", "helper"] {
            symlink(
                "../bobr-bundle-launcher",
                temp.path().join("libexec/wrapped-bin").join(name),
            )
            .unwrap();
            config.tools.insert(
                name.to_string(),
                ToolConfig {
                    path: format!("root/usr/bin/{name}"),
                    argv0: name.to_string(),
                    visibility: ToolVisibility::Internal,
                    argument_prefix: Vec::new(),
                    environment: BTreeMap::new(),
                },
            );
        }
        fs::write(temp.path().join("bundle.toml"), config.to_toml().unwrap()).unwrap();

        let structure = verify_structure(temp.path(), &config).unwrap();
        verify_startup_closure(temp.path(), &config, &structure).unwrap();

        fs::write(
            temp.path().join("root/usr/bin/demo"),
            b"#!/usr/bin/env -S helper --flag\n",
        )
        .unwrap();
        let structure = verify_structure(temp.path(), &config).unwrap();
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(
            error.contains("accepts exactly one managed command"),
            "{error}"
        );
    }

    #[test]
    fn accepts_origin_paths_and_rejects_host_or_unknown_dynamic_paths() {
        let temp = tempdir().unwrap();
        let payload = temp.path().join("root");
        let object_dir = payload.join("usr/bin");
        let library_dir = payload.join("usr/lib");
        fs::create_dir_all(&object_dir).unwrap();
        fs::create_dir_all(&library_dir).unwrap();
        let object = object_dir.join("demo");

        assert_eq!(
            expand_dynamic_paths(
                &["$ORIGIN/../lib".to_string()],
                &object_dir,
                &payload,
                "DT_RUNPATH",
                &object,
            )
            .unwrap(),
            [library_dir]
        );
        for forbidden in ["/usr/lib", "relative/lib", "$LIB", "$ORIGIN/$PLATFORM"] {
            let error = expand_dynamic_paths(
                &[forbidden.to_string()],
                &object_dir,
                &payload,
                "DT_RUNPATH",
                &object,
            )
            .unwrap_err();
            assert!(error.contains("unsupported DT_RUNPATH"), "{error}");
        }
    }

    #[test]
    fn resolves_a_dynamic_startup_closure_and_rejects_a_missing_library() {
        let (temp, config) = fixture();
        dynamic_elf(
            &temp.path().join("root/usr/bin/demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libdemo.so.1"],
            None,
        );
        fs::create_dir_all(temp.path().join("root/lib64")).unwrap();
        static_elf(&temp.path().join("root/lib64/ld-linux-x86-64.so.2"));
        dynamic_elf(
            &temp.path().join("root/usr/lib/libdemo.so.1"),
            None,
            &["libchild.so.1"],
            Some("libdemo.so.1"),
        );
        dynamic_elf(
            &temp.path().join("root/usr/lib/libchild.so.1"),
            None,
            &[],
            Some("libchild.so.1"),
        );
        let structure = verify_structure(temp.path(), &config).unwrap();
        verify_startup_closure(temp.path(), &config, &structure).unwrap();

        fs::remove_file(temp.path().join("root/usr/lib/libchild.so.1")).unwrap();
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(error.contains("cannot resolve DT_NEEDED"), "{error}");
        assert!(error.contains("libchild.so.1"), "{error}");
    }

    #[test]
    fn accepts_empty_library_dirs_only_for_a_static_startup_closure() {
        let (temp, mut config) = fixture();
        config.loader.library_dirs.clear();
        write_config(temp.path(), &config);
        let structure = verify_structure(temp.path(), &config).unwrap();
        verify_startup_closure(temp.path(), &config, &structure).unwrap();

        dynamic_elf(
            &temp.path().join("root/usr/bin/demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &[],
            None,
        );
        fs::create_dir_all(temp.path().join("root/lib64")).unwrap();
        static_elf(&temp.path().join("root/lib64/ld-linux-x86-64.so.2"));
        let structure = verify_structure(temp.path(), &config).unwrap();
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(error.contains("library_dirs is empty"), "{error}");
    }

    #[test]
    fn rejects_bundle_toml_that_drifted_after_validation() {
        let (temp, config) = fixture();
        let text = fs::read_to_string(temp.path().join("bundle.toml")).unwrap();
        fs::write(
            temp.path().join("bundle.toml"),
            text.replace("min_kernel = \"4.19\"", "min_kernel = \"6.1\""),
        )
        .unwrap();
        let error = verify_structure(temp.path(), &config).unwrap_err();
        assert!(error.contains("differs from the validated runtime config"));
    }

    #[test]
    fn verifier_and_launcher_share_the_shebang_depth_limit() {
        let (temp, config) = fixture();
        let bin = temp.path().join("root/usr/bin");
        script(&bin.join("demo"), "/usr/bin/one");
        script(&bin.join("one"), "/usr/bin/two");
        script(&bin.join("two"), "/usr/bin/three");
        script(&bin.join("three"), "/usr/bin/final");
        static_elf(&bin.join("final"));

        let structure = verify_structure(temp.path(), &config).unwrap();
        verify_startup_closure(temp.path(), &config, &structure).unwrap();

        script(&bin.join("three"), "/usr/bin/four");
        script(&bin.join("four"), "/usr/bin/final");
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(
            error.contains(&format!("shebang recursion limit ({MAX_SCRIPT_DEPTH})")),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_dependency_with_the_wrong_soname() {
        let (temp, config) = fixture();
        dynamic_elf(
            &temp.path().join("root/usr/bin/demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libdemo.so.1"],
            None,
        );
        fs::create_dir_all(temp.path().join("root/lib64")).unwrap();
        static_elf(&temp.path().join("root/lib64/ld-linux-x86-64.so.2"));
        dynamic_elf(
            &temp.path().join("root/usr/lib/libdemo.so.1"),
            None,
            &[],
            Some("libother.so.1"),
        );

        let structure = verify_structure(temp.path(), &config).unwrap();
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(error.contains("with SONAME 'libother.so.1'"), "{error}");
    }

    #[test]
    fn accepts_pt_interp_in_a_dt_needed_shared_object() {
        let (temp, config) = fixture();
        let loader = "/lib64/ld-linux-x86-64.so.2";
        dynamic_elf(
            &temp.path().join("root/usr/bin/demo"),
            Some(loader),
            &["libc.so.6"],
            None,
        );
        fs::create_dir_all(temp.path().join("root/lib64")).unwrap();
        static_elf(&temp.path().join("root/lib64/ld-linux-x86-64.so.2"));
        let libc = temp.path().join("root/usr/lib/libc.so.6");
        dynamic_elf(&libc, Some(loader), &[], Some("libc.so.6"));
        set_elf_type(&libc, 3);

        let structure = verify_structure(temp.path(), &config).unwrap();
        verify_startup_closure(temp.path(), &config, &structure).unwrap();
    }

    #[test]
    fn reports_missing_gnu_symbol_versions() {
        let required = BTreeSet::from(["GLIBC_2.38".to_string(), "GLIBC_2.39".to_string()]);
        let provided = BTreeSet::from(["GLIBC_2.38".to_string()]);
        let error = verify_symbol_versions(
            Path::new("/bundle/root/usr/bin/demo"),
            "libc.so.6",
            Path::new("/bundle/root/usr/lib/libc.so.6"),
            Some(&required),
            &provided,
        )
        .unwrap_err();
        assert!(error.contains("GLIBC_2.39"), "{error}");
        assert!(!error.contains("GLIBC_2.38, GLIBC_2.39"), "{error}");
    }

    #[test]
    fn follows_glibc_rpath_runpath_order_and_inheritance() {
        let (temp, config) = fixture();
        let root = temp.path().join("root");
        let bin = root.join("usr/bin");
        let global = root.join("usr/lib");
        let rpath = root.join("usr/rpath");
        let runpath = root.join("usr/runpath");
        fs::create_dir_all(&rpath).unwrap();
        fs::create_dir_all(&runpath).unwrap();
        fs::create_dir_all(root.join("lib64")).unwrap();
        static_elf(&root.join("lib64/ld-linux-x86-64.so.2"));

        dynamic_elf_with_paths(
            &bin.join("demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libparent.so.1"],
            None,
            Some("$ORIGIN/../rpath"),
            None,
        );
        dynamic_elf(
            &global.join("libparent.so.1"),
            None,
            &["libchild.so.1"],
            Some("libparent.so.1"),
        );
        dynamic_elf(
            &rpath.join("libchild.so.1"),
            None,
            &[],
            Some("libchild.so.1"),
        );
        let structure = verify_structure(temp.path(), &config).unwrap();
        verify_startup_closure(temp.path(), &config, &structure).unwrap();

        dynamic_elf_with_paths(
            &bin.join("demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libparent.so.1"],
            None,
            Some("$ORIGIN/../rpath"),
            Some("$ORIGIN/../runpath"),
        );
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(error.contains("libchild.so.1"), "{error}");

        dynamic_elf(&rpath.join("libparent.so.1"), None, &[], Some("wrong.so.1"));
        dynamic_elf_with_paths(
            &bin.join("demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libparent.so.1"],
            None,
            Some("$ORIGIN/../rpath"),
            None,
        );
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(error.contains("with SONAME 'wrong.so.1'"), "{error}");

        dynamic_elf_with_paths(
            &bin.join("demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libparent.so.1"],
            None,
            Some("$ORIGIN/../rpath"),
            Some("$ORIGIN/../runpath"),
        );
        dynamic_elf(
            &global.join("libparent.so.1"),
            None,
            &[],
            Some("libparent.so.1"),
        );
        verify_startup_closure(temp.path(), &config, &structure).unwrap();
    }

    #[test]
    fn memoizes_library_audits_by_path_and_inherited_rpath_context() {
        let (temp, _) = fixture();
        let payload_root = fs::canonicalize(temp.path().join("root")).unwrap();
        let library = payload_root.join("usr/lib/libdemo.so.1");
        dynamic_elf(&library, None, &[], Some("libdemo.so.1"));
        let mut verifier = StartupVerifier {
            payload_root,
            library_dirs: Vec::new(),
            managed_tools: BTreeMap::new(),
            platform_arch: PlatformArch::X86_64,
            active_executables: BTreeSet::new(),
            active_libraries: BTreeSet::new(),
            verified_executables: BTreeSet::new(),
            verified_libraries: BTreeSet::new(),
            visited_states: 0,
        };
        for _ in 0..MAX_STARTUP_NODES + 1 {
            verifier.audit_library(&library, &[]).unwrap();
        }
        assert_eq!(verifier.visited_states, 1);

        let inherited = vec![verifier.payload_root.join("usr/other")];
        verifier.audit_library(&library, &inherited).unwrap();
        assert_eq!(verifier.visited_states, 2);
    }

    #[test]
    fn rejects_a_dangling_library_candidate_before_a_later_match() {
        let (temp, mut config) = fixture();
        let root = temp.path().join("root");
        let early = root.join("usr/early");
        let later = root.join("usr/later");
        fs::create_dir_all(&early).unwrap();
        fs::create_dir_all(&later).unwrap();
        fs::create_dir_all(root.join("lib64")).unwrap();
        config.loader.library_dirs =
            vec!["root/usr/early".to_string(), "root/usr/later".to_string()];
        write_config(temp.path(), &config);
        dynamic_elf(
            &root.join("usr/bin/demo"),
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libdemo.so.1"],
            None,
        );
        static_elf(&root.join("lib64/ld-linux-x86-64.so.2"));
        symlink("missing-target", early.join("libdemo.so.1")).unwrap();
        dynamic_elf(&later.join("libdemo.so.1"), None, &[], Some("libdemo.so.1"));

        let structure = verify_structure(temp.path(), &config).unwrap();
        let error = verify_startup_closure(temp.path(), &config, &structure).unwrap_err();
        assert!(error.contains("failed to resolve DT_NEEDED"), "{error}");
        assert!(error.contains("root/usr/early/libdemo.so.1"), "{error}");
    }

    fn dynamic_elf(path: &Path, interpreter: Option<&str>, needed: &[&str], soname: Option<&str>) {
        dynamic_elf_with_paths(path, interpreter, needed, soname, None, None);
    }

    fn set_elf_type(path: &Path, elf_type: u16) {
        let mut bytes = fs::read(path).unwrap();
        bytes[16..18].copy_from_slice(&elf_type.to_le_bytes());
        fs::write(path, bytes).unwrap();
    }

    fn dynamic_elf_with_paths(
        path: &Path,
        interpreter: Option<&str>,
        needed: &[&str],
        soname: Option<&str>,
        rpath: Option<&str>,
        runpath: Option<&str>,
    ) {
        const BASE: u64 = 0x400000;
        const PT_LOAD: u32 = 1;
        const PT_DYNAMIC: u32 = 2;
        const PT_INTERP: u32 = 3;
        const DT_NULL: u64 = 0;
        const DT_NEEDED: u64 = 1;
        const DT_STRTAB: u64 = 5;
        const DT_STRSZ: u64 = 10;
        const DT_SONAME: u64 = 14;
        const DT_RPATH: u64 = 15;
        const DT_RUNPATH: u64 = 29;

        let phnum = 2 + usize::from(interpreter.is_some());
        let headers_end = 64 + phnum * 56;
        let interpreter_bytes = interpreter.map(|value| {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            bytes
        });
        let interpreter_offset = headers_end;
        let dynamic_offset = interpreter_offset + interpreter_bytes.as_ref().map_or(0, Vec::len);

        let mut strings = vec![0_u8];
        let needed_offsets = needed
            .iter()
            .map(|value| {
                let offset = strings.len();
                strings.extend_from_slice(value.as_bytes());
                strings.push(0);
                offset
            })
            .collect::<Vec<_>>();
        let soname_offset = soname.map(|value| {
            let offset = strings.len();
            strings.extend_from_slice(value.as_bytes());
            strings.push(0);
            offset
        });
        let rpath_offset = rpath.map(|value| {
            let offset = strings.len();
            strings.extend_from_slice(value.as_bytes());
            strings.push(0);
            offset
        });
        let runpath_offset = runpath.map(|value| {
            let offset = strings.len();
            strings.extend_from_slice(value.as_bytes());
            strings.push(0);
            offset
        });
        let dynamic_count = 3
            + needed.len()
            + usize::from(soname.is_some())
            + usize::from(rpath.is_some())
            + usize::from(runpath.is_some());
        let string_offset = dynamic_offset + dynamic_count * 16;
        let file_size = string_offset + strings.len();
        let mut bytes = vec![0_u8; file_size];

        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        let elf_type = if interpreter.is_some() { 2_u16 } else { 3_u16 };
        bytes[16..18].copy_from_slice(&elf_type.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());

        write_program_header(
            &mut bytes,
            64,
            PT_LOAD,
            0,
            BASE,
            file_size as u64,
            file_size as u64,
        );
        let mut ph_index = 1;
        if let Some(interpreter_bytes) = &interpreter_bytes {
            write_program_header(
                &mut bytes,
                64 + ph_index * 56,
                PT_INTERP,
                interpreter_offset as u64,
                BASE + interpreter_offset as u64,
                interpreter_bytes.len() as u64,
                interpreter_bytes.len() as u64,
            );
            bytes[interpreter_offset..dynamic_offset].copy_from_slice(interpreter_bytes);
            ph_index += 1;
        }
        write_program_header(
            &mut bytes,
            64 + ph_index * 56,
            PT_DYNAMIC,
            dynamic_offset as u64,
            BASE + dynamic_offset as u64,
            (dynamic_count * 16) as u64,
            (dynamic_count * 16) as u64,
        );

        let mut dynamic_index = 0;
        write_dynamic(
            &mut bytes,
            dynamic_offset,
            &mut dynamic_index,
            DT_STRTAB,
            BASE + string_offset as u64,
        );
        write_dynamic(
            &mut bytes,
            dynamic_offset,
            &mut dynamic_index,
            DT_STRSZ,
            strings.len() as u64,
        );
        for offset in needed_offsets {
            write_dynamic(
                &mut bytes,
                dynamic_offset,
                &mut dynamic_index,
                DT_NEEDED,
                offset as u64,
            );
        }
        if let Some(offset) = soname_offset {
            write_dynamic(
                &mut bytes,
                dynamic_offset,
                &mut dynamic_index,
                DT_SONAME,
                offset as u64,
            );
        }
        if let Some(offset) = rpath_offset {
            write_dynamic(
                &mut bytes,
                dynamic_offset,
                &mut dynamic_index,
                DT_RPATH,
                offset as u64,
            );
        }
        if let Some(offset) = runpath_offset {
            write_dynamic(
                &mut bytes,
                dynamic_offset,
                &mut dynamic_index,
                DT_RUNPATH,
                offset as u64,
            );
        }
        write_dynamic(&mut bytes, dynamic_offset, &mut dynamic_index, DT_NULL, 0);
        bytes[string_offset..].copy_from_slice(&strings);
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write_program_header(
        bytes: &mut [u8],
        offset: usize,
        kind: u32,
        file_offset: u64,
        virtual_address: u64,
        file_size: u64,
        memory_size: u64,
    ) {
        bytes[offset..offset + 4].copy_from_slice(&kind.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&5_u32.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&file_offset.to_le_bytes());
        bytes[offset + 16..offset + 24].copy_from_slice(&virtual_address.to_le_bytes());
        bytes[offset + 24..offset + 32].copy_from_slice(&virtual_address.to_le_bytes());
        bytes[offset + 32..offset + 40].copy_from_slice(&file_size.to_le_bytes());
        bytes[offset + 40..offset + 48].copy_from_slice(&memory_size.to_le_bytes());
        bytes[offset + 48..offset + 56].copy_from_slice(&1_u64.to_le_bytes());
    }

    fn write_dynamic(
        bytes: &mut [u8],
        dynamic_offset: usize,
        index: &mut usize,
        tag: u64,
        value: u64,
    ) {
        let offset = dynamic_offset + *index * 16;
        bytes[offset..offset + 8].copy_from_slice(&tag.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&value.to_le_bytes());
        *index += 1;
    }
}
