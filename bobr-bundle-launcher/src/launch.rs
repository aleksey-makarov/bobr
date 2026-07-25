//! Final process replacement for prepared HostBundle launch plans.

use crate::{ProcessEnvironment, ResolvedTool};
use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Replaces the launcher with a static payload executable.
///
/// This function returns only when `execve` fails.
pub fn exec_static(
    tool: &ResolvedTool,
    args: &[OsString],
    environment: &ProcessEnvironment,
) -> io::Error {
    let mut command = Command::new(tool.target());
    command
        .arg0(&tool.config().argv0)
        .args(args)
        .env_clear()
        .envs(environment.iter());
    command.exec()
}
