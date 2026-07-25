//! Command-line entry point for `bobr-bundle-launcher`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_bundle_launcher::{
    build_environment, locate_current_bundle, parse_invocation, read_bundle_config, resolve_tool,
};

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
    let tool = match resolve_tool(&location, &config, invocation.tool()) {
        Ok(tool) => tool,
        Err(error) => exit_with_error(error),
    };
    let environment = match build_environment(&location, &config, &tool, std::env::vars_os()) {
        Ok(environment) => environment,
        Err(error) => exit_with_error(error),
    };
    eprintln!(
        "bobr-bundle-launcher: launching is not implemented yet; target is '{}', environment has {} variables, invocation is {invocation:?}",
        tool.target().display(),
        environment.len()
    );
    std::process::exit(2)
}

fn exit_with_error(error: impl std::fmt::Display) -> ! {
    eprintln!("bobr-bundle-launcher: {error}");
    std::process::exit(2)
}
