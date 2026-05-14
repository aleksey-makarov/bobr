use mbuild_sandbox_runner_core::{protocol_info, run_config_path};
use std::path::Path;

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() == 2 && args[1] == "--protocol-info" {
        match serde_json::to_string(&protocol_info()) {
            Ok(json) => {
                println!("{json}");
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("failed to serialize protocol info: {error}");
                std::process::exit(2);
            }
        }
    }

    if args.len() != 2 {
        eprintln!("usage: mbuild-sandbox-runner <runner-config.json>");
        std::process::exit(2);
    }

    std::process::exit(run_config_path(Path::new(&args[1])));
}
