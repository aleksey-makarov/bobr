//! Command-line entry point for `bobr-bundle-launcher`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_bundle_launcher::{
    ElfLinkage, build_environment, exec_dynamic, exec_static, inspect_elf, locate_current_bundle,
    parse_invocation, prepare_dynamic_launch, read_bundle_config, resolve_tool,
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
    let dynamic_plan = match elf.linkage() {
        ElfLinkage::Static => None,
        ElfLinkage::Dynamic { interpreter } => Some(
            match prepare_dynamic_launch(&location, &config, &tool, interpreter, invocation.args())
            {
                Ok(plan) => plan,
                Err(error) => exit_with_error(error),
            },
        ),
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
        if let Some(plan) = &dynamic_plan {
            println!("loader={}", plan.loader().display());
        }
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
            let error = exec_dynamic(
                dynamic_plan
                    .as_ref()
                    .expect("dynamic ELF must have a prepared loader plan"),
                &environment,
            );
            exit_with_error(format!(
                "failed to execute bundled loader for '{}': {error}",
                tool.name()
            ));
        }
    }
}

fn exit_with_error(error: impl std::fmt::Display) -> ! {
    eprintln!("bobr-bundle-launcher: {error}");
    std::process::exit(2)
}
