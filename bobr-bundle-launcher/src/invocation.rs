//! Command-line and multi-call invocation parsing.

use crate::LAUNCHER_BINARY_NAME;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::Path;

/// A validated request made to the bundle launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Run the tool selected by the basename of `argv[0]`.
    MultiCall {
        /// Tool name looked up in `bundle.toml`.
        tool: String,
        /// Arguments passed to the tool, excluding its `argv[0]`.
        args: Vec<OsString>,
    },
    /// Run an explicitly named tool.
    Run {
        /// Tool name looked up in `bundle.toml`.
        tool: String,
        /// Arguments passed after the required `--` separator.
        args: Vec<OsString>,
    },
    /// Describe how an explicitly named tool would be launched.
    Diagnose {
        /// Tool name looked up in `bundle.toml`.
        tool: String,
    },
}

/// Invalid bundle-launcher command-line syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationError {
    message: String,
}

impl InvocationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for InvocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for InvocationError {}

/// Parses launcher arguments, including `argv[0]`.
///
/// Invoking the binary through a symlink selects multi-call mode. Invoking it
/// as `bobr-bundle-launcher` requires either `--run TOOL -- ARGS...` or
/// `--diagnose TOOL`.
pub fn parse_invocation(
    args: impl IntoIterator<Item = OsString>,
) -> Result<Invocation, InvocationError> {
    let args = args.into_iter().collect::<Vec<_>>();
    let Some(argv0) = args.first() else {
        return Err(InvocationError::new("argv is empty"));
    };
    let executable_name = Path::new(argv0)
        .file_name()
        .ok_or_else(|| InvocationError::new("argv[0] has no executable name"))?;

    if executable_name != OsStr::new(LAUNCHER_BINARY_NAME) {
        return Ok(Invocation::MultiCall {
            tool: parse_tool_name(executable_name, "argv[0]")?,
            args: args[1..].to_vec(),
        });
    }

    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--run") => parse_run(&args),
        Some("--diagnose") => parse_diagnose(&args),
        _ => Err(usage_error()),
    }
}

fn parse_run(args: &[OsString]) -> Result<Invocation, InvocationError> {
    if args.len() < 4 || args[3] != OsStr::new("--") {
        return Err(usage_error());
    }
    Ok(Invocation::Run {
        tool: parse_tool_name(&args[2], "--run tool")?,
        args: args[4..].to_vec(),
    })
}

fn parse_diagnose(args: &[OsString]) -> Result<Invocation, InvocationError> {
    if args.len() != 3 {
        return Err(usage_error());
    }
    Ok(Invocation::Diagnose {
        tool: parse_tool_name(&args[2], "--diagnose tool")?,
    })
}

fn parse_tool_name(value: &OsStr, field: &str) -> Result<String, InvocationError> {
    let value = value
        .to_str()
        .ok_or_else(|| InvocationError::new(format!("{field} is not valid UTF-8")))?;
    if value.is_empty() || value == "." || value == ".." || value.as_bytes().contains(&b'/') {
        return Err(InvocationError::new(format!(
            "{field} must be a non-empty basename"
        )));
    }
    Ok(value.to_string())
}

fn usage_error() -> InvocationError {
    InvocationError::new(format!(
        "usage: {LAUNCHER_BINARY_NAME} --run TOOL -- [ARGS...]\n       \
         {LAUNCHER_BINARY_NAME} --diagnose TOOL"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_multicall_invocation_from_absolute_symlink_path() {
        let invocation =
            parse_invocation(args(&["/store/bundle/bin/qemu-img", "info", "disk.img"])).unwrap();

        assert_eq!(
            invocation,
            Invocation::MultiCall {
                tool: "qemu-img".to_string(),
                args: args(&["info", "disk.img"]),
            }
        );
    }

    #[test]
    fn multicall_preserves_non_utf8_payload_arguments() {
        let opaque = OsString::from_vec(vec![0xff, 0xfe]);

        let invocation =
            parse_invocation(vec![OsString::from("qemu-img"), opaque.clone()]).unwrap();

        assert_eq!(
            invocation,
            Invocation::MultiCall {
                tool: "qemu-img".to_string(),
                args: vec![opaque],
            }
        );
    }

    #[test]
    fn parses_explicit_run_and_strips_separator() {
        let invocation = parse_invocation(args(&[
            LAUNCHER_BINARY_NAME,
            "--run",
            "qemu-system-x86_64",
            "--",
            "-machine",
            "q35",
        ]))
        .unwrap();

        assert_eq!(
            invocation,
            Invocation::Run {
                tool: "qemu-system-x86_64".to_string(),
                args: args(&["-machine", "q35"]),
            }
        );
    }

    #[test]
    fn parses_explicit_run_without_payload_arguments() {
        let invocation =
            parse_invocation(args(&[LAUNCHER_BINARY_NAME, "--run", "qemu-img", "--"])).unwrap();

        assert_eq!(
            invocation,
            Invocation::Run {
                tool: "qemu-img".to_string(),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_diagnose() {
        let invocation =
            parse_invocation(args(&[LAUNCHER_BINARY_NAME, "--diagnose", "qemu-img"])).unwrap();

        assert_eq!(
            invocation,
            Invocation::Diagnose {
                tool: "qemu-img".to_string(),
            }
        );
    }

    #[test]
    fn rejects_non_utf8_tool_name() {
        let non_utf8 = OsString::from_vec(vec![0xff]);
        let error = parse_invocation(vec![
            OsString::from(LAUNCHER_BINARY_NAME),
            OsString::from("--run"),
            non_utf8,
            OsString::from("--"),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn rejects_explicit_tool_path() {
        let error = parse_invocation(args(&[
            LAUNCHER_BINARY_NAME,
            "--run",
            "usr/bin/qemu-img",
            "--",
        ]))
        .unwrap_err();

        assert!(error.to_string().contains("basename"));
    }

    #[test]
    fn rejects_run_without_separator() {
        let error =
            parse_invocation(args(&[LAUNCHER_BINARY_NAME, "--run", "qemu-img"])).unwrap_err();

        assert!(error.to_string().contains("usage:"));
    }

    #[test]
    fn rejects_diagnose_with_extra_arguments() {
        let error = parse_invocation(args(&[
            LAUNCHER_BINARY_NAME,
            "--diagnose",
            "qemu-img",
            "extra",
        ]))
        .unwrap_err();

        assert!(error.to_string().contains("usage:"));
    }

    #[test]
    fn rejects_direct_launcher_invocation_without_mode() {
        let error = parse_invocation(args(&[LAUNCHER_BINARY_NAME])).unwrap_err();

        assert!(error.to_string().contains("--run"));
    }

    #[test]
    fn rejects_empty_argv() {
        let error = parse_invocation(Vec::new()).unwrap_err();

        assert_eq!(error.to_string(), "argv is empty");
    }

    #[test]
    fn rejects_multicall_name_that_is_not_utf8() {
        let error = parse_invocation(vec![OsString::from_vec(vec![0xff])]).unwrap_err();

        assert!(error.to_string().contains("argv[0]"));
    }

    #[test]
    fn multicall_does_not_interpret_payload_options() {
        let invocation = parse_invocation(args(&["qemu-img", "--diagnose", "payload"])).unwrap();

        assert_eq!(
            invocation,
            Invocation::MultiCall {
                tool: "qemu-img".to_string(),
                args: args(&["--diagnose", "payload"]),
            }
        );
    }

    #[test]
    fn tool_validation_uses_raw_path_separator_byte() {
        assert!(parse_tool_name(OsStr::from_bytes(b"a/b"), "tool").is_err());
    }
}
