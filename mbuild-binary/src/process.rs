use mbuild_core::BuildContext;
use std::io;
use std::process::{Command as ProcessCommand, Output, Stdio};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub(crate) enum CommandOutcome {
    Finished(Output),
    CancelledBeforeStart,
    Cancelled(Output),
}

pub(crate) fn run_cancellable_command(
    mut command: ProcessCommand,
    cx: &BuildContext,
) -> io::Result<CommandOutcome> {
    if cx.cancellation_token().is_cancelled() {
        return Ok(CommandOutcome::CancelledBeforeStart);
    }

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    prepare_child_group(&mut command);
    let mut child = command.spawn()?;

    loop {
        if cx.cancellation_token().is_cancelled() {
            kill_child_group(&mut child);
            let output = child.wait_with_output()?;
            return Ok(CommandOutcome::Cancelled(output));
        }

        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            return Ok(CommandOutcome::Finished(output));
        }

        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn prepare_child_group(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn prepare_child_group(_command: &mut ProcessCommand) {}

#[cfg(unix)]
fn kill_child_group(child: &mut std::process::Child) {
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_child_group(child: &mut std::process::Child) {
    let _ = child.kill();
}
