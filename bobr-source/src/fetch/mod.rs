//! `bobr-fetch`: downloads a request's sources into the store, so the build
//! that follows finds every one already present and does no recipe-driven
//! network work of its own.
//!
//! The command line is a request path and `--version`; everything else --
//! store, run directories, limits, the sources themselves -- arrives in the
//! request (see [`request::FETCH_REQUEST_SCHEMA`]).

mod engine;
mod request;

pub use engine::{Mismatch, Summary, run_fetch};
pub use request::{FETCH_REQUEST_SCHEMA, FetchRequest, Limits, SourceEntry};

use std::process::ExitCode;

/// The whole binary: parses arguments, runs the request, turns the summary
/// into an exit code. Lives in the library so the binary stays one line.
pub fn main() -> ExitCode {
    let arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--version") {
        println!(
            "bobr-fetch {} (request {})",
            env!("CARGO_PKG_VERSION"),
            FETCH_REQUEST_SCHEMA
        );
        return ExitCode::SUCCESS;
    }
    let [request_path] = arguments.as_slice() else {
        eprintln!("usage: bobr-fetch REQUEST.json | bobr-fetch --version");
        return ExitCode::from(2);
    };

    let request = match std::fs::read(request_path)
        .map_err(|error| format!("failed to read '{}': {error}", request_path.display()))
        .and_then(|bytes| FetchRequest::parse_json(&bytes))
    {
        Ok(request) => request,
        Err(message) => {
            eprintln!("bobr-fetch: {message}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bobr-fetch: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run_fetch(request)) {
        Ok(summary) if summary.is_success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("bobr-fetch: {message}");
            ExitCode::FAILURE
        }
    }
}

use std::path::Path;

trait OsStrPath {
    fn display(&self) -> std::path::Display<'_>;
}

impl OsStrPath for std::ffi::OsString {
    fn display(&self) -> std::path::Display<'_> {
        Path::new(self).display()
    }
}
