use clap::{Parser, Subcommand};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::process::ExitCode;

const ROOT_DIR: &str = ".mbuild";
const MATERIALIZED_DIR: &str = "materialized";
const BIN_OUT_DIR: &str = "bin-out";

#[derive(Parser, Debug)]
#[command(name = "mbbin")]
#[command(about = "Minimal binary builder (MVP)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Verify that podman CLI is available.
    Doctor,
    /// Run a fixed smoke demo in a temporary container.
    RunDemo {
        /// Container image.
        #[arg(long, default_value = "docker.io/library/alpine:3.20")]
        image: String,
    },
    /// Run an arbitrary command in a temporary container.
    Run {
        /// Container image.
        #[arg(long, default_value = "docker.io/library/alpine:3.20")]
        image: String,
        /// Command to execute inside container (use `--` before command).
        #[arg(required = true, trailing_var_arg = true)]
        cmd: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(1)
        }
    }
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Doctor => run_doctor(),
        Command::RunDemo { image } => run_demo(&image),
        Command::Run { image, cmd } => run_container(&image, &cmd),
    }
}

fn run_doctor() -> Result<(), String> {
    let output = ProcessCommand::new("podman")
        .arg("--version")
        .output()
        .map_err(|error| format!("failed to execute podman: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            "podman returned non-zero status without output".to_string()
        };
        return Err(format!("podman --version failed: {details}"));
    }

    println!("doctor: ok");
    println!("{}", String::from_utf8_lossy(&output.stdout).trim());
    Ok(())
}

fn run_demo(image: &str) -> Result<(), String> {
    let cmd = vec![
        "sh".to_string(),
        "-lc".to_string(),
        "echo hello-from-mbbin > /out/hello.txt && ls -la /in && ls -la /out".to_string(),
    ];
    run_container(image, &cmd)
}

fn run_container(image: &str, cmd: &[String]) -> Result<(), String> {
    let layout = workspace_layout()?;
    ensure_base_dirs(&layout)?;

    let in_mount = format!("{}:/in:ro", layout.materialized.display());
    let out_mount = format!("{}:/out:rw", layout.bin_out.display());

    let mut process = ProcessCommand::new("podman");
    process
        .arg("run")
        .arg("--rm")
        .arg("--volume")
        .arg(in_mount)
        .arg("--volume")
        .arg(out_mount)
        .arg(image)
        .args(cmd);

    let output = process
        .output()
        .map_err(|error| format!("failed to execute podman run: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            "podman run failed without output".to_string()
        };
        return Err(format!("podman run failed: {details}"));
    }

    println!("run: ok");
    println!("image: {image}");
    println!("input_mount: {}", layout.materialized.display());
    println!("output_mount: {}", layout.bin_out.display());
    if !output.stdout.is_empty() {
        println!("{}", String::from_utf8_lossy(&output.stdout).trim_end());
    }
    Ok(())
}

struct WorkspaceLayout {
    root: PathBuf,
    materialized: PathBuf,
    bin_out: PathBuf,
}

fn workspace_layout() -> Result<WorkspaceLayout, String> {
    let cwd =
        env::current_dir().map_err(|error| format!("failed to get current directory: {error}"))?;
    let root = cwd.join(ROOT_DIR);
    let materialized = root.join(MATERIALIZED_DIR);
    let bin_out = root.join(BIN_OUT_DIR);

    Ok(WorkspaceLayout {
        root,
        materialized,
        bin_out,
    })
}

fn ensure_base_dirs(layout: &WorkspaceLayout) -> Result<(), String> {
    ensure_dir(&layout.root, "mbuild root")?;
    ensure_dir(&layout.materialized, "materialized")?;
    ensure_dir(&layout.bin_out, "bin-out")?;
    Ok(())
}

fn ensure_dir(path: &PathBuf, label: &str) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "failed to create or access {label} directory '{}': {error}",
            path.display()
        )
    })
}
