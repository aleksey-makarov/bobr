use crate::error::RuntimeError;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// User identity used for a sandbox step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRunAs {
    /// Run as the sandbox build user, currently numeric `1:1`.
    BuildUser,
    /// Run as container root, currently numeric `0:0`.
    Root,
}

/// A named input mount passed to a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxInput {
    /// Recipe input name. Must match `[A-Za-z_][A-Za-z0-9_]*`.
    pub name: String,
    /// Host path to the realized input object.
    pub host_path: PathBuf,
}

/// One command step to execute inside a sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxStep {
    /// Step name.
    pub name: String,
    /// User identity for the step.
    pub run_as: SandboxRunAs,
    /// Absolute working directory inside the sandbox.
    pub cwd: PathBuf,
    /// Argument vector to execute.
    pub argv: Vec<String>,
    /// Environment variables for the step.
    pub env: HashMap<String, String>,
    /// Host log path for stdout.
    pub stdout_path: PathBuf,
    /// Host log path for stderr.
    pub stderr_path: PathBuf,
}

/// Runtime configuration for one sandbox build.
#[derive(Debug, Clone)]
pub struct SandboxBuildConfig {
    /// Prepared root filesystem directory used as the sandbox root base.
    pub root_dir: PathBuf,
    /// Host directory for build output.
    pub out_dir: PathBuf,
    /// Host directory for script config.
    pub config_dir: PathBuf,
    /// Host workspace for temporary runtime state.
    pub workspace: PathBuf,
    /// Additional named inputs.
    pub inputs: Vec<SandboxInput>,
    /// Ordered build steps.
    pub steps: Vec<SandboxStep>,
}

pub(super) fn validate_config(config: &SandboxBuildConfig) -> Result<(), RuntimeError> {
    require_directory(&config.root_dir, "sandbox root directory")?;
    require_directory(&config.out_dir, "sandbox output directory")?;
    require_directory(&config.config_dir, "sandbox config directory")?;
    require_directory(&config.workspace, "sandbox workspace")?;
    validate_root_dir_top_level(&config.root_dir)?;
    let mut input_names = HashSet::new();
    for input in &config.inputs {
        validate_input_name(&input.name)?;
        if !input_names.insert(input.name.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox input '{}' is defined more than once",
                input.name
            )));
        }
        if !input.host_path.is_dir() && !input.host_path.is_file() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox input '{}' must resolve to a file or directory: '{}'",
                input.name,
                input.host_path.display()
            )));
        }
    }
    validate_steps(&config.steps)?;
    Ok(())
}

fn validate_input_name(name: &str) -> Result<(), RuntimeError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(RuntimeError::InvalidInput(
            "sandbox input name must not be empty".to_string(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(RuntimeError::InvalidInput(format!(
            "sandbox input name '{name}' must start with an ASCII letter or underscore"
        )));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(RuntimeError::InvalidInput(format!(
            "sandbox input name '{name}' must contain only ASCII letters, digits, and underscores"
        )));
    }
    Ok(())
}

fn validate_steps(steps: &[SandboxStep]) -> Result<(), RuntimeError> {
    let mut names = HashSet::new();
    let mut log_paths = HashSet::new();
    for (index, step) in steps.iter().enumerate() {
        if step.name.is_empty() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox step at index {index} name must not be empty"
            )));
        }
        if !names.insert(step.name.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox step '{}' is defined more than once",
                step.name
            )));
        }
        if !step.cwd.is_absolute() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox step '{}' cwd must be absolute: '{}'",
                step.name,
                step.cwd.display()
            )));
        }
        if step.argv.is_empty() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox step '{}' argv must not be empty",
                step.name
            )));
        }
        if step.argv[0].is_empty() {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox step '{}' argv[0] must not be empty",
                step.name
            )));
        }
        for (stream, path) in [("stdout", &step.stdout_path), ("stderr", &step.stderr_path)] {
            if !log_paths.insert(path.clone()) {
                return Err(RuntimeError::InvalidInput(format!(
                    "sandbox step '{}' {stream} log path is not unique: '{}'",
                    step.name,
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_root_dir_top_level(root_dir: &Path) -> Result<(), RuntimeError> {
    reject_reserved_rootfs_entry(root_dir, "__mbuild")?;
    for entry in root_dir_top_level_entries(root_dir)? {
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "sandbox root directory '{}' contains non-UTF-8 top-level entry",
                root_dir.display()
            ))
        })?;
        let file_type = entry.file_type()?;
        if !(file_type.is_file() || file_type.is_dir() || file_type.is_symlink()) {
            return Err(RuntimeError::InvalidInput(format!(
                "sandbox root directory '{}' top-level entry '/{name}' must be a file, directory, or symlink",
                root_dir.display()
            )));
        }
    }
    Ok(())
}

fn root_dir_top_level_entries(root_dir: &Path) -> Result<Vec<fs::DirEntry>, RuntimeError> {
    fs::read_dir(root_dir)?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(RuntimeError::from)
}

fn require_directory(path: &Path, label: &str) -> Result<(), RuntimeError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "{label} '{}' must exist and be a directory",
            path.display()
        )))
    }
}

fn reject_reserved_rootfs_entry(rootfs: &Path, name: &str) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(rootfs.join(name)) {
        Ok(_) => Err(RuntimeError::InvalidInput(format!(
            "sandbox rootfs '{}' contains reserved top-level entry '/{name}'",
            rootfs.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::Io(error)),
    }
}

#[cfg(test)]
pub(super) fn valid_config(temp: &tempfile::TempDir) -> SandboxBuildConfig {
    let root_dir = temp.path().join("rootfs");
    let out_dir = temp.path().join("out");
    let config_dir = temp.path().join("config");
    let workspace = temp.path().join("workspace");
    for path in [&root_dir, &out_dir, &config_dir, &workspace] {
        fs::create_dir_all(path).unwrap();
    }
    SandboxBuildConfig {
        root_dir,
        out_dir,
        config_dir,
        workspace,
        inputs: Vec::new(),
        steps: Vec::new(),
    }
}

#[cfg(test)]
pub(super) fn runtime_step(temp: &tempfile::TempDir, name: &str) -> SandboxStep {
    SandboxStep {
        name: name.to_string(),
        run_as: SandboxRunAs::BuildUser,
        cwd: PathBuf::from("/"),
        argv: vec!["true".to_string()],
        env: HashMap::new(),
        stdout_path: temp.path().join(format!("{name}.stdout")),
        stderr_path: temp.path().join(format!("{name}.stderr")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use tempfile::tempdir;

    #[test]
    fn sandbox_config_rejects_reserved_mbuild_rootfs_entry() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let out = temp.path().join("out");
        let config = temp.path().join("config");
        let workspace = temp.path().join("workspace");
        for path in [&rootfs, &out, &config, &workspace] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir(rootfs.join("__mbuild")).unwrap();
        let build_config = SandboxBuildConfig {
            root_dir: rootfs.clone(),
            out_dir: out,
            config_dir: config,
            workspace,
            inputs: Vec::new(),
            steps: Vec::new(),
        };

        let error = validate_config(&build_config).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains("reserved top-level entry"));
    }

    #[test]
    fn sandbox_config_rejects_invalid_input_names() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        fs::create_dir(&input).unwrap();
        let mut config = valid_config(&temp);
        config.inputs.push(SandboxInput {
            name: "bad-name".to_string(),
            host_path: input,
        });

        let error = validate_config(&config).unwrap_err();

        assert!(error.to_string().contains("must contain only ASCII"));
    }

    #[test]
    fn sandbox_config_rejects_invalid_steps_and_duplicate_logs() {
        let temp = tempdir().unwrap();
        let mut config = valid_config(&temp);
        config.steps = vec![runtime_step(&temp, "build")];
        config.steps[0].cwd = PathBuf::from("relative");
        let error = validate_config(&config).unwrap_err();
        assert!(error.to_string().contains("cwd must be absolute"));

        let mut config = valid_config(&temp);
        config.steps = vec![runtime_step(&temp, "build")];
        config.steps[0].argv.clear();
        let error = validate_config(&config).unwrap_err();
        assert!(error.to_string().contains("argv must not be empty"));

        let mut config = valid_config(&temp);
        config.steps = vec![runtime_step(&temp, "build")];
        config.steps[0].argv[0].clear();
        let error = validate_config(&config).unwrap_err();
        assert!(error.to_string().contains("argv[0] must not be empty"));

        let mut config = valid_config(&temp);
        config.steps = vec![runtime_step(&temp, "build"), runtime_step(&temp, "build")];
        let error = validate_config(&config).unwrap_err();
        assert!(error.to_string().contains("defined more than once"));

        let mut config = valid_config(&temp);
        let mut step = runtime_step(&temp, "build");
        step.stderr_path = step.stdout_path.clone();
        config.steps = vec![step];
        let error = validate_config(&config).unwrap_err();
        assert!(error.to_string().contains("log path is not unique"));
    }

    #[test]
    fn sandbox_config_rejects_unsupported_root_top_level_file_types() {
        let temp = tempdir().unwrap();
        let config = valid_config(&temp);
        let fifo = config.root_dir.join("fifo");
        let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o644) };
        assert_eq!(result, 0, "mkfifo failed: {}", io::Error::last_os_error());

        let error = validate_config(&config).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("must be a file, directory, or symlink")
        );
    }
}
