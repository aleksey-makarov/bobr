use indicatif::{ProgressBar, ProgressStyle};
use std::io::{self, IsTerminal};
use std::time::Duration;

/// Human-readable progress for long administrative repository operations.
///
/// Machine-readable command results stay on stdout. An interactive stderr gets
/// one continuously refreshed spinner, while redirected stderr receives only
/// phase transitions and final informational messages.
pub(crate) struct RepositoryProgress {
    bar: Option<ProgressBar>,
    plain: bool,
}

impl RepositoryProgress {
    pub(crate) fn new(quiet: bool) -> Self {
        if quiet {
            return Self {
                bar: None,
                plain: false,
            };
        }
        if io::stderr().is_terminal() {
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template("{spinner:.cyan} repository: {wide_msg}")
                    .expect("static repository progress template"),
            );
            bar.enable_steady_tick(Duration::from_millis(100));
            Self {
                bar: Some(bar),
                plain: false,
            }
        } else {
            Self {
                bar: None,
                plain: true,
            }
        }
    }

    pub(crate) fn phase(&self, message: impl Into<String>) {
        let message = message.into();
        if let Some(bar) = &self.bar {
            bar.set_message(message);
        } else if self.plain {
            eprintln!("repository: {message}");
        }
    }

    pub(crate) fn detail(&self, message: impl Into<String>) {
        if let Some(bar) = &self.bar {
            bar.set_message(message.into());
        }
    }

    pub(crate) fn info(&self, message: impl AsRef<str>) {
        if let Some(bar) = &self.bar {
            bar.println(message.as_ref());
        } else {
            eprintln!("{}", message.as_ref());
        }
    }

    pub(crate) fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

impl Drop for RepositoryProgress {
    fn drop(&mut self) {
        self.finish();
    }
}
