//! Command-line entry point for `bobr-bundle-launcher`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_bundle_launcher::{
    ElfLinkage, ExecutableFormat, build_environment, exec_prepared, locate_current_bundle,
    parse_invocation, prepare_tool_launch, read_bundle_config, resolve_tool,
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
    let launch = match prepare_tool_launch(&location, &config, &tool, invocation.args()) {
        Ok(launch) => launch,
        Err(error) => exit_with_error(error),
    };
    if invocation.is_diagnose() {
        println!("tool={}", tool.name());
        println!("target={}", tool.target().display());
        match launch.format() {
            ExecutableFormat::Elf(elf) => {
                println!(
                    "linkage={}",
                    match elf.linkage() {
                        ElfLinkage::Static => "static",
                        ElfLinkage::Dynamic { .. } => "dynamic",
                    }
                );
            }
            ExecutableFormat::Script(shebang) => {
                println!("linkage=script");
                println!("interpreter={}", shebang.interpreter().display());
            }
        }
        println!("environment_variables={}", environment.len());
        if let Some(loader) = launch.process().loader() {
            println!("loader={}", loader.display());
        }
        return;
    }

    let error = exec_prepared(&launch, &environment);
    exit_with_error(format!("failed to execute tool '{}': {error}", tool.name()));
}

fn exit_with_error(error: impl std::fmt::Display) -> ! {
    eprintln!("bobr-bundle-launcher: {error}");
    std::process::exit(2)
}
