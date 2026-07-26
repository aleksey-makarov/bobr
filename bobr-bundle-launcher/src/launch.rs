//! Final process replacement for prepared HostBundle launch plans.

use crate::{DynamicLaunchPlan, PreparedToolLaunch, ProcessEnvironment};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Replaces the launcher with a prepared bundled dynamic loader.
///
/// This function returns only when `execve` fails.
pub(crate) fn exec_dynamic(
    plan: &DynamicLaunchPlan,
    environment: &ProcessEnvironment,
) -> io::Error {
    let mut command = Command::new(plan.loader());
    command
        .args(plan.arguments())
        .env_clear()
        .envs(environment.iter());
    command.exec()
}

/// Replaces the launcher with a completely prepared ELF or script command.
///
/// This function returns only when `execve` fails.
pub fn exec_prepared(launch: &PreparedToolLaunch, environment: &ProcessEnvironment) -> io::Error {
    let process = launch.process();
    if let Some((executable, argv0, arguments)) = process.direct_parts() {
        let mut command = Command::new(executable);
        command
            .arg0(argv0)
            .args(arguments)
            .env_clear()
            .envs(environment.iter());
        return command.exec();
    }
    let dynamic = process
        .dynamic()
        .expect("a non-direct process launch plan must be dynamic");
    exec_dynamic(dynamic, environment)
}
