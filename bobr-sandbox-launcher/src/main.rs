use bobr_sandbox_launcher::{launch, protocol_info};
use std::os::fd::RawFd;
use std::path::PathBuf;

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

    match parse_launch_args(&args) {
        Ok((wait_fd, config_path)) => {
            std::process::exit(launch(wait_fd, &config_path));
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

fn parse_launch_args(args: &[String]) -> Result<(RawFd, PathBuf), String> {
    if args.len() != 6 || args[1] != "launch" || args[2] != "--wait-fd" || args[4] != "--config" {
        return Err("usage: bobr-sandbox-launcher launch --wait-fd FD --config PATH".to_string());
    }
    let wait_fd = args[3]
        .parse::<RawFd>()
        .map_err(|error| format!("invalid --wait-fd '{}': {error}", args[3]))?;
    Ok((wait_fd, PathBuf::from(&args[5])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_launch_args() {
        let args = vec![
            "bobr-sandbox-launcher".to_string(),
            "launch".to_string(),
            "--wait-fd".to_string(),
            "7".to_string(),
            "--config".to_string(),
            "/tmp/config.json".to_string(),
        ];

        let (fd, path) = parse_launch_args(&args).unwrap();

        assert_eq!(fd, 7);
        assert_eq!(path, PathBuf::from("/tmp/config.json"));
    }

    #[test]
    fn rejects_old_bare_runner_config_mode() {
        let args = vec![
            "bobr-sandbox-launcher".to_string(),
            "/tmp/runner-config.json".to_string(),
        ];

        assert!(parse_launch_args(&args).unwrap_err().contains("launch"));
    }
}
