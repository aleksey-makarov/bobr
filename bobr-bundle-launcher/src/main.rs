//! Command-line entry point for `bobr-bundle-launcher`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_bundle_launcher::{
    DiagnosticOutput, DiagnosticReport, build_environment, check_host_platform, exec_prepared,
    locate_current_bundle, parse_invocation, prepare_tool_launch, read_bundle_config, resolve_tool,
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
    let host_platform = match check_host_platform(&config.platform) {
        Ok(platform) => platform,
        Err(error) => exit_with_error(error),
    };
    if !invocation.is_diagnose() && !host_platform.is_compatible() {
        exit_with_error(format!(
            "host platform does not satisfy bundle requirements \
             (required linux/{} kernel >= {}, host kernel {})",
            config.platform.arch,
            config.platform.min_kernel,
            host_platform.kernel_release()
        ));
    }
    let tool = match resolve_tool(&location, &config, invocation.tool()) {
        Ok(tool) => tool,
        Err(error) => exit_with_error(error),
    };
    let environment = match build_environment(&location, &config, &tool, std::env::vars_os()) {
        Ok(environment) => environment,
        Err(error) => exit_with_error(error),
    };
    let launch = match prepare_tool_launch(&location, &config, &tool, invocation.args()) {
        Ok(launch) => launch,
        Err(error) => exit_with_error(error),
    };
    if invocation.is_diagnose() {
        let report = DiagnosticReport::new(
            &location,
            &config,
            &tool,
            &environment,
            &launch,
            &host_platform,
        );
        match invocation
            .diagnostic_output()
            .expect("diagnostic invocation has an output format")
        {
            DiagnosticOutput::Human => println!("{}", report.to_human()),
            DiagnosticOutput::Json => println!("{}", report.to_json()),
        }
        return;
    }

    let error = exec_prepared(&launch, &environment);
    eprintln!(
        "bobr-bundle-launcher: failed to execute tool '{}': {error}",
        tool.name()
    );
    std::process::exit(match error.kind() {
        std::io::ErrorKind::NotFound => 127,
        _ => 126,
    });
}

fn exit_with_error(error: impl std::fmt::Display) -> ! {
    eprintln!("bobr-bundle-launcher: {error}");
    std::process::exit(2)
}
