//! Command-line entry point for `bobr-bundle-launcher`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_bundle_launcher::{
    ElfLinkage, build_environment, exec_static, inspect_elf, locate_current_bundle,
    parse_invocation, read_bundle_config, resolve_tool,
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
    let elf = match inspect_elf(tool.target()) {
        Ok(elf) => elf,
        Err(error) => exit_with_error(error),
    };
    if invocation.is_diagnose() {
        println!("tool={}", tool.name());
        println!("target={}", tool.target().display());
        println!(
            "linkage={}",
            match elf.linkage() {
                ElfLinkage::Static => "static",
                ElfLinkage::Dynamic { .. } => "dynamic",
            }
        );
        println!("environment_variables={}", environment.len());
        return;
    }

    match elf.linkage() {
        ElfLinkage::Static => {
            let error = exec_static(&tool, invocation.args(), &environment);
            exit_with_error(format!(
                "failed to execute static tool '{}': {error}",
                tool.name()
            ));
        }
        ElfLinkage::Dynamic { .. } => {
            exit_with_error(format!(
                "dynamic tool '{}' is not supported yet",
                tool.name()
            ));
        }
    }
}

fn exit_with_error(error: impl std::fmt::Display) -> ! {
    eprintln!("bobr-bundle-launcher: {error}");
    std::process::exit(2)
}
