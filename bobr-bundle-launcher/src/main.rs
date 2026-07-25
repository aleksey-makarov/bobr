//! Command-line entry point for `bobr-bundle-launcher`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_bundle_launcher::{locate_current_bundle, parse_invocation, read_bundle_config};

fn main() {
    let invocation = match parse_invocation(std::env::args_os()) {
        Ok(invocation) => invocation,
        Err(error) => exit_with_error(error),
    };
    let location = match locate_current_bundle() {
        Ok(location) => location,
        Err(error) => exit_with_error(error),
    };
    let config = match read_bundle_config(&location.config()) {
        Ok(config) => config,
        Err(error) => exit_with_error(error),
    };
    eprintln!(
        "bobr-bundle-launcher: launching is not implemented yet; bundle root is '{}', format is '{}', invocation is {invocation:?}",
        location.root().display(),
        config.format()
    );
    std::process::exit(2)
}

fn exit_with_error(error: impl std::fmt::Display) -> ! {
    eprintln!("bobr-bundle-launcher: {error}");
    std::process::exit(2)
}
