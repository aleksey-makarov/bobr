use crate::error::RuntimeError;
use mbuild_sandbox_runner_core::{RUNNER_BINARY_NAME, RUNNER_PROTOCOL_VERSION, RunnerProtocolInfo};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

#[derive(Debug)]
pub(super) struct SandboxTools {
    pub(super) runner: SandboxRunnerBinary,
    pub(super) newuidmap: PathBuf,
    pub(super) newgidmap: PathBuf,
}

#[derive(Debug)]
pub(super) struct SandboxRunnerBinary {
    pub(super) host_path: PathBuf,
}

pub(super) fn cached_sandbox_tools() -> Result<Arc<SandboxTools>, RuntimeError> {
    static TOOLS: OnceLock<Result<Arc<SandboxTools>, String>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            resolve_and_preflight_sandbox_tools()
                .map(Arc::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|message| RuntimeError::Preflight(message.clone()))
}

fn resolve_and_preflight_sandbox_tools() -> Result<SandboxTools, RuntimeError> {
    Ok(SandboxTools {
        runner: resolve_and_preflight_sandbox_runner()?,
        newuidmap: resolve_path_program(OsStr::new("newuidmap"))?,
        newgidmap: resolve_path_program(OsStr::new("newgidmap"))?,
    })
}

fn resolve_and_preflight_sandbox_runner() -> Result<SandboxRunnerBinary, RuntimeError> {
    let host_path = resolve_sandbox_runner_path()?;
    require_executable_file(&host_path, "sandbox runner")?;
    require_static_elf(&host_path)?;
    let output = Command::new(&host_path)
        .arg("--protocol-info")
        .output()
        .map_err(|error| {
            RuntimeError::Preflight(format!(
                "failed to run sandbox runner preflight '{} --protocol-info': {error}",
                host_path.display()
            ))
        })?;
    if !output.status.success() {
        return Err(RuntimeError::Preflight(format!(
            "sandbox runner preflight '{} --protocol-info' failed with status {}: {}",
            host_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let info = serde_json::from_slice::<RunnerProtocolInfo>(&output.stdout).map_err(|error| {
        RuntimeError::Preflight(format!(
            "failed to parse sandbox runner protocol info from '{}': {error}",
            host_path.display()
        ))
    })?;
    if info.name != RUNNER_BINARY_NAME || info.protocol_version != RUNNER_PROTOCOL_VERSION {
        return Err(RuntimeError::Preflight(format!(
            "sandbox runner '{}' has incompatible protocol {:?}; expected name '{}' protocol {}",
            host_path.display(),
            info,
            RUNNER_BINARY_NAME,
            RUNNER_PROTOCOL_VERSION
        )));
    }
    Ok(SandboxRunnerBinary { host_path })
}

fn resolve_sandbox_runner_path() -> Result<PathBuf, RuntimeError> {
    resolve_sandbox_runner_path_from(
        env::var_os("MBUILD_SANDBOX_RUNNER").map(PathBuf::from),
        env::current_exe().ok().as_deref(),
        env::var_os("PATH"),
    )
}

fn resolve_sandbox_runner_path_from(
    env_override: Option<PathBuf>,
    current_exe: Option<&Path>,
    path_env: Option<OsString>,
) -> Result<PathBuf, RuntimeError> {
    let mut checked = Vec::new();
    if let Some(path) = env_override {
        checked.push(path.clone());
        if path.exists() {
            return Ok(path);
        }
    }

    if let Some(current_exe) = current_exe {
        if let Some((target_dir, profile)) = cargo_target_dir_and_profile(current_exe) {
            let candidate = target_dir
                .join("x86_64-unknown-linux-musl")
                .join(profile)
                .join(RUNNER_BINARY_NAME);
            checked.push(candidate.clone());
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join(RUNNER_BINARY_NAME);
            checked.push(sibling.clone());
            if sibling.exists() {
                return Ok(sibling);
            }
        }
        for ancestor in current_exe.ancestors() {
            let target_dir = ancestor.join("target");
            for profile in ["debug", "release"] {
                let candidate = target_dir
                    .join("x86_64-unknown-linux-musl")
                    .join(profile)
                    .join(RUNNER_BINARY_NAME);
                checked.push(candidate.clone());
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    if let Some(path) = path_env {
        for dir in env::split_paths(&path) {
            let candidate = dir.join(RUNNER_BINARY_NAME);
            checked.push(candidate.clone());
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Err(RuntimeError::Preflight(format!(
        "failed to find sandbox runner '{}'; checked {}",
        RUNNER_BINARY_NAME,
        checked
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

fn cargo_target_dir_and_profile(current_exe: &Path) -> Option<(&Path, &str)> {
    let profile_dir = current_exe.parent()?;
    let profile = profile_dir.file_name()?.to_str()?;
    if !matches!(profile, "debug" | "release") {
        return None;
    }
    let parent = profile_dir.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("target") {
        return Some((parent, profile));
    }
    let target_dir = parent.parent()?;
    if target_dir.file_name().and_then(|name| name.to_str()) == Some("target") {
        return Some((target_dir, profile));
    }
    None
}

fn resolve_path_program(name: &OsStr) -> Result<PathBuf, RuntimeError> {
    let Some(path_env) = env::var_os("PATH") else {
        return Err(RuntimeError::Preflight(format!(
            "{} not found: PATH is unset",
            name.to_string_lossy()
        )));
    };
    for dir in env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.exists() {
            require_executable_file(&candidate, &name.to_string_lossy())?;
            return Ok(candidate);
        }
    }
    Err(RuntimeError::Preflight(format!(
        "{} not found in PATH",
        name.to_string_lossy()
    )))
}

fn require_executable_file(path: &Path, label: &str) -> Result<(), RuntimeError> {
    let metadata = fs::metadata(path).map_err(|error| {
        RuntimeError::Preflight(format!(
            "{label} '{}' cannot be inspected: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(RuntimeError::Preflight(format!(
            "{label} '{}' is not a file",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(RuntimeError::Preflight(format!(
            "{label} '{}' is not executable",
            path.display()
        )));
    }
    Ok(())
}

fn require_static_elf(path: &Path) -> Result<(), RuntimeError> {
    let bytes = fs::read(path).map_err(|error| {
        RuntimeError::Preflight(format!(
            "failed to read sandbox runner '{}': {error}",
            path.display()
        ))
    })?;
    match elf_has_interpreter(&bytes) {
        Ok(true) => Err(RuntimeError::Preflight(format!(
            "sandbox runner '{}' is dynamically linked; build a static musl runner",
            path.display()
        ))),
        Ok(false) => Ok(()),
        Err(message) => Err(RuntimeError::Preflight(format!(
            "failed to inspect sandbox runner ELF '{}': {message}",
            path.display()
        ))),
    }
}

fn elf_has_interpreter(bytes: &[u8]) -> Result<bool, String> {
    const PT_INTERP: u32 = 3;
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        return Err("not an ELF file".to_string());
    }
    if bytes[4] != 2 {
        return Err("unsupported non-64-bit ELF".to_string());
    }
    if bytes[5] != 1 {
        return Err("unsupported non-little-endian ELF".to_string());
    }
    let phoff = read_u64_le(bytes, 32)? as usize;
    let phentsize = read_u16_le(bytes, 54)? as usize;
    let phnum = read_u16_le(bytes, 56)? as usize;
    if phentsize < 4 {
        return Err("invalid ELF program header size".to_string());
    }
    for index in 0..phnum {
        let offset = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or("ELF program headers overflow")?,
            )
            .ok_or("ELF program headers overflow")?;
        let end = offset
            .checked_add(phentsize)
            .ok_or("ELF program header overflow")?;
        if end > bytes.len() {
            return Err("ELF program header outside file".to_string());
        }
        if read_u32_le(bytes, offset)? == PT_INTERP {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or("unexpected end of ELF file")?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or("unexpected end of ELF file")?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or("unexpected end of ELF file")?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sandbox_runner_resolution_prefers_musl_runner_in_cargo_dev_tree() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let debug = target.join("debug");
        let musl_debug = target.join("x86_64-unknown-linux-musl").join("debug");
        fs::create_dir_all(&debug).unwrap();
        fs::create_dir_all(&musl_debug).unwrap();
        let current_exe = debug.join("mbuild");
        let dynamic_sibling = debug.join(RUNNER_BINARY_NAME);
        let static_runner = musl_debug.join(RUNNER_BINARY_NAME);
        fs::write(&current_exe, "").unwrap();
        fs::write(&dynamic_sibling, "").unwrap();
        fs::write(&static_runner, "").unwrap();

        let resolved = resolve_sandbox_runner_path_from(None, Some(&current_exe), None).unwrap();

        assert_eq!(resolved, static_runner);
    }

    #[test]
    fn sandbox_runner_resolution_uses_installed_sibling_outside_cargo_tree() {
        let temp = tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let current_exe = bin.join("mbuild");
        let sibling = bin.join(RUNNER_BINARY_NAME);
        fs::write(&current_exe, "").unwrap();
        fs::write(&sibling, "").unwrap();

        let resolved = resolve_sandbox_runner_path_from(None, Some(&current_exe), None).unwrap();

        assert_eq!(resolved, sibling);
    }

    #[test]
    fn elf_interpreter_detection_finds_dynamic_runner_shape() {
        assert!(elf_has_interpreter(&minimal_elf64_with_program_header(3)).unwrap());
        assert!(!elf_has_interpreter(&minimal_elf64_with_program_header(1)).unwrap());
    }

    fn minimal_elf64_with_program_header(program_type: u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; 64 + 56];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[32..40].copy_from_slice(&(64_u64).to_le_bytes());
        bytes[54..56].copy_from_slice(&(56_u16).to_le_bytes());
        bytes[56..58].copy_from_slice(&(1_u16).to_le_bytes());
        bytes[64..68].copy_from_slice(&program_type.to_le_bytes());
        bytes
    }
}
