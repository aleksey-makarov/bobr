//! Recursive ELF and shebang dispatch inside one HostBundle payload.

use crate::{
    BundleConfig, BundleLocation, DynamicLaunchError, DynamicLaunchPlan, ElfLinkage,
    ExecutableFormat, ExecutableInspectionError, PlatformArch, ResolvedTool,
    inspect_executable_for_arch, prepare_dynamic_program,
};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Maximum number of script nodes accepted in one launcher dispatch chain.
///
/// The builder imports this value so its startup verifier accepts exactly the
/// same shebang subset as the runtime launcher.
pub const MAX_SCRIPT_DEPTH: usize = 4;

/// Final process command after recursively resolving any shebang interpreters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessLaunchPlan {
    /// Enter an ELF without `PT_INTERP` directly.
    Direct {
        /// Canonical executable path.
        executable: PathBuf,
        /// Logical `argv[0]`.
        argv0: OsString,
        /// Arguments after `argv[0]`.
        arguments: Vec<OsString>,
    },
    /// Enter an ELF through its validated bundled glibc loader.
    Dynamic(DynamicLaunchPlan),
}

impl ProcessLaunchPlan {
    /// Returns the final loader path when this command is dynamic.
    pub fn loader(&self) -> Option<&Path> {
        match self {
            Self::Direct { .. } => None,
            Self::Dynamic(plan) => Some(plan.loader()),
        }
    }

    pub(crate) fn direct_parts(&self) -> Option<(&Path, &OsStr, &[OsString])> {
        match self {
            Self::Direct {
                executable,
                argv0,
                arguments,
            } => Some((executable, argv0, arguments)),
            Self::Dynamic(_) => None,
        }
    }

    pub(crate) fn dynamic(&self) -> Option<&DynamicLaunchPlan> {
        match self {
            Self::Direct { .. } => None,
            Self::Dynamic(plan) => Some(plan),
        }
    }
}

/// A configured tool and its completely resolved process command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedToolLaunch {
    format: ExecutableFormat,
    process: ProcessLaunchPlan,
}

impl PreparedToolLaunch {
    /// Returns the configured tool's original on-disk format.
    pub fn format(&self) -> &ExecutableFormat {
        &self.format
    }

    /// Returns the final process command.
    pub fn process(&self) -> &ProcessLaunchPlan {
        &self.process
    }
}

/// Failure to resolve an ELF or script entirely within the payload.
#[derive(Debug)]
pub enum DispatchError {
    /// A configured tool or recursive interpreter could not be inspected.
    Inspect {
        /// Path whose format was inspected.
        path: PathBuf,
        /// Format inspection failure.
        source: ExecutableInspectionError,
    },
    /// More nested shebang scripts were encountered than Linux accepts.
    ScriptRecursionLimit(PathBuf),
    /// A bundled shebang interpreter could not be resolved.
    ResolveInterpreter {
        /// Logical shebang path.
        interpreter: PathBuf,
        /// Candidate path under the payload.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A shebang interpreter symlink resolves outside the payload.
    InterpreterEscapesPayload {
        /// Logical shebang path.
        interpreter: PathBuf,
        /// Canonical payload root.
        payload_root: PathBuf,
        /// Canonical escaped target.
        target: PathBuf,
    },
    /// A shebang interpreter is not an executable regular file.
    InvalidInterpreterFile {
        /// Logical shebang path.
        interpreter: PathBuf,
        /// Canonical resolved target.
        target: PathBuf,
    },
    /// Dynamic-loader planning failed.
    Dynamic(DynamicLaunchError),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect { path, source } => {
                write!(
                    formatter,
                    "failed to inspect bundled executable '{}': {source}",
                    path.display()
                )
            }
            Self::ScriptRecursionLimit(path) => write!(
                formatter,
                "shebang recursion limit ({MAX_SCRIPT_DEPTH}) exceeded at '{}'",
                path.display()
            ),
            Self::ResolveInterpreter {
                interpreter,
                path,
                source,
            } => write!(
                formatter,
                "failed to resolve bundled shebang interpreter '{}' at '{}': {source}",
                interpreter.display(),
                path.display()
            ),
            Self::InterpreterEscapesPayload {
                interpreter,
                payload_root,
                target,
            } => write!(
                formatter,
                "shebang interpreter '{}' resolves to '{}' outside payload root '{}'",
                interpreter.display(),
                target.display(),
                payload_root.display()
            ),
            Self::InvalidInterpreterFile {
                interpreter,
                target,
            } => write!(
                formatter,
                "bundled shebang interpreter '{}' at '{}' is not an executable regular file",
                interpreter.display(),
                target.display()
            ),
            Self::Dynamic(error) => error.fmt(formatter),
        }
    }
}

impl Error for DispatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspect { source, .. } => Some(source),
            Self::ResolveInterpreter { source, .. } => Some(source),
            Self::Dynamic(error) => Some(error),
            _ => None,
        }
    }
}

/// Resolves the complete launch chain for one configured tool.
pub fn prepare_tool_launch(
    location: &BundleLocation,
    bundle: &BundleConfig,
    tool: &ResolvedTool,
    arguments: &[OsString],
) -> Result<PreparedToolLaunch, DispatchError> {
    let format = inspect(tool.target(), bundle.platform.arch)?;
    let mut complete_arguments = Vec::with_capacity(tool.argument_prefix().len() + arguments.len());
    complete_arguments.extend_from_slice(tool.argument_prefix());
    complete_arguments.extend_from_slice(arguments);
    let process = prepare_inspected(
        location,
        bundle,
        tool.payload_root(),
        tool.target(),
        OsStr::new(&tool.config().argv0),
        &complete_arguments,
        &format,
        0,
    )?;
    Ok(PreparedToolLaunch { format, process })
}

#[allow(clippy::too_many_arguments)]
fn prepare_inspected(
    location: &BundleLocation,
    bundle: &BundleConfig,
    payload_root: &Path,
    target: &Path,
    argv0: &OsStr,
    arguments: &[OsString],
    format: &ExecutableFormat,
    script_depth: usize,
) -> Result<ProcessLaunchPlan, DispatchError> {
    match format {
        ExecutableFormat::Elf(elf) => match elf.linkage() {
            ElfLinkage::Static => Ok(ProcessLaunchPlan::Direct {
                executable: target.to_path_buf(),
                argv0: argv0.to_os_string(),
                arguments: arguments.to_vec(),
            }),
            ElfLinkage::Dynamic { interpreter } => prepare_dynamic_program(
                location,
                bundle,
                payload_root,
                target,
                argv0,
                interpreter,
                arguments,
            )
            .map(ProcessLaunchPlan::Dynamic)
            .map_err(DispatchError::Dynamic),
        },
        ExecutableFormat::Script(shebang) => {
            if script_depth >= MAX_SCRIPT_DEPTH {
                return Err(DispatchError::ScriptRecursionLimit(target.to_path_buf()));
            }
            let interpreter = resolve_script_interpreter(payload_root, shebang.interpreter())?;
            let interpreter_format = inspect(&interpreter, bundle.platform.arch)?;
            let mut interpreter_arguments =
                Vec::with_capacity(arguments.len() + usize::from(shebang.argument().is_some()) + 1);
            if let Some(argument) = shebang.argument() {
                interpreter_arguments.push(argument.to_os_string());
            }
            interpreter_arguments.push(target.as_os_str().to_os_string());
            interpreter_arguments.extend_from_slice(arguments);
            prepare_inspected(
                location,
                bundle,
                payload_root,
                &interpreter,
                // Like the kernel, a script replaces configured argv[0] with
                // the logical shebang interpreter name. The canonical script
                // target becomes the next argument.
                shebang.interpreter().as_os_str(),
                &interpreter_arguments,
                &interpreter_format,
                script_depth + 1,
            )
        }
    }
}

fn inspect(path: &Path, expected_arch: PlatformArch) -> Result<ExecutableFormat, DispatchError> {
    inspect_executable_for_arch(path, expected_arch).map_err(|source| DispatchError::Inspect {
        path: path.to_path_buf(),
        source,
    })
}

fn resolve_script_interpreter(
    payload_root: &Path,
    logical: &Path,
) -> Result<PathBuf, DispatchError> {
    let relative = crate::dynamic::validate_absolute_payload_path(logical)
        .expect("validated Shebang must contain a safe absolute path");
    let candidate = payload_root.join(relative);
    let target =
        fs::canonicalize(&candidate).map_err(|source| DispatchError::ResolveInterpreter {
            interpreter: logical.to_path_buf(),
            path: candidate,
            source,
        })?;
    if !target.starts_with(payload_root) {
        return Err(DispatchError::InterpreterEscapesPayload {
            interpreter: logical.to_path_buf(),
            payload_root: payload_root.to_path_buf(),
            target,
        });
    }
    let metadata = fs::metadata(&target).map_err(|source| DispatchError::ResolveInterpreter {
        interpreter: logical.to_path_buf(),
        path: target.clone(),
        source,
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(DispatchError::InvalidInterpreterFile {
            interpreter: logical.to_path_buf(),
            target,
        });
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BundleConfig, locate_bundle_from_launcher, resolve_tool};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _temp: tempfile::TempDir,
        location: BundleLocation,
        config: BundleConfig,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("bundle");
            fs::create_dir_all(root.join("libexec")).unwrap();
            fs::create_dir_all(root.join("root/usr/bin")).unwrap();
            fs::create_dir_all(root.join("root/bin")).unwrap();
            let location =
                locate_bundle_from_launcher(&root.join("libexec/bobr-bundle-launcher")).unwrap();
            let config = BundleConfig::parse(
                r#"
format = "bobr-host-bundle-v2"
payload_root = "root"
policy = "strict"
[platform]
os = "linux"
arch = "x86_64"
min_kernel = "4.19"
[loader]
kind = "glibc"
library_dirs = []
inhibit_cache = true
[tools.demo]
path = "root/usr/bin/demo"
argv0 = "logical-demo"
visibility = "public"
"#,
            )
            .unwrap();
            Self {
                _temp: temp,
                location,
                config,
                root,
            }
        }

        fn write_executable(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn tool(&self) -> ResolvedTool {
            resolve_tool(&self.location, &self.config, "demo").unwrap()
        }
    }

    #[test]
    fn script_plan_reproduces_shebang_argument_order() {
        let fixture = Fixture::new();
        fixture.write_executable(
            "root/usr/bin/demo",
            b"#!/bin/interpreter optional argument\n",
        );
        let interpreter = fixture.write_executable("root/bin/interpreter", &minimal_static_elf());
        let tool = fixture.tool();

        let launch = prepare_tool_launch(
            &fixture.location,
            &fixture.config,
            &tool,
            &[OsString::from("caller-argument")],
        )
        .unwrap();

        let (executable, argv0, arguments) = launch.process().direct_parts().unwrap();
        assert_eq!(executable, interpreter);
        assert_eq!(argv0, OsStr::new("/bin/interpreter"));
        assert_eq!(
            arguments,
            [
                OsString::from("optional argument"),
                tool.target().as_os_str().to_os_string(),
                OsString::from("caller-argument"),
            ]
        );
    }

    #[test]
    fn fixed_arguments_precede_caller_arguments_for_scripts() {
        let mut fixture = Fixture::new();
        fixture.write_executable("root/usr/bin/demo", b"#!/bin/interpreter\n");
        let interpreter = fixture.write_executable("root/bin/interpreter", &minimal_static_elf());
        fs::create_dir_all(fixture.root.join("root/usr/share/qemu")).unwrap();
        fixture
            .config
            .tools
            .get_mut("demo")
            .unwrap()
            .argument_prefix = vec![
            crate::ToolArgument::Literal {
                value: "-L".to_string(),
            },
            crate::ToolArgument::Path {
                path: "root/usr/share/qemu".to_string(),
            },
        ];
        let tool = fixture.tool();

        let launch = prepare_tool_launch(
            &fixture.location,
            &fixture.config,
            &tool,
            &[OsString::from("caller")],
        )
        .unwrap();

        let (executable, _, arguments) = launch.process().direct_parts().unwrap();
        assert_eq!(executable, interpreter);
        assert_eq!(arguments[0], tool.target().as_os_str());
        assert_eq!(arguments[1], OsStr::new("-L"));
        assert_eq!(
            arguments[2],
            fixture.root.join("root/usr/share/qemu").as_os_str()
        );
        assert_eq!(arguments[3], OsStr::new("caller"));
    }

    #[test]
    fn accepts_exactly_four_nested_scripts() {
        let fixture = Fixture::new();
        fixture.write_executable("root/usr/bin/demo", b"#!/bin/one\n");
        fixture.write_executable("root/bin/one", b"#!/bin/two\n");
        fixture.write_executable("root/bin/two", b"#!/bin/three\n");
        fixture.write_executable("root/bin/three", b"#!/bin/final\n");
        let final_executable = fixture.write_executable("root/bin/final", &minimal_static_elf());

        let launch =
            prepare_tool_launch(&fixture.location, &fixture.config, &fixture.tool(), &[]).unwrap();

        assert_eq!(launch.process().direct_parts().unwrap().0, final_executable);
    }

    #[test]
    fn rejects_more_than_four_nested_scripts() {
        let fixture = Fixture::new();
        fixture.write_executable("root/usr/bin/demo", b"#!/bin/one\n");
        fixture.write_executable("root/bin/one", b"#!/bin/two\n");
        fixture.write_executable("root/bin/two", b"#!/bin/three\n");
        fixture.write_executable("root/bin/three", b"#!/bin/four\n");
        fixture.write_executable("root/bin/four", b"#!/bin/five\n");
        fixture.write_executable("root/bin/five", &minimal_static_elf());

        let error = prepare_tool_launch(&fixture.location, &fixture.config, &fixture.tool(), &[])
            .unwrap_err();

        assert!(matches!(error, DispatchError::ScriptRecursionLimit(_)));
    }

    fn minimal_static_elf() -> Vec<u8> {
        let mut elf = vec![0_u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[6] = 1;
        elf[16..18].copy_from_slice(&2_u16.to_le_bytes());
        elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1_u32.to_le_bytes());
        elf[32..40].copy_from_slice(&64_u64.to_le_bytes());
        elf[52..54].copy_from_slice(&64_u16.to_le_bytes());
        elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
        elf
    }
}
