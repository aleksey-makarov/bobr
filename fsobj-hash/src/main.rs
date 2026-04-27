use fsobj_hash::{hash_path, hash_tar_file, hash_tar_reader};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

const USAGE: &str = "usage: fsobj-hash <path> [--mode=auto|direct|tar]";
const HELP: &str = "\
usage: fsobj-hash <path> [--mode=auto|direct|tar]

Compute the filesystem object hash of a path or tar stream.

Arguments:
  <path>    Filesystem path to hash, or '-' with --mode=tar for tar on stdin

Options:
  --mode    Hashing mode (default: auto)
            auto   hash directories directly, '.tar' files as tar archives,
                   and other files directly
            direct hash the filesystem path as-is
            tar    hash the path as a tar archive, or read tar from stdin
  -h, --help
            Show this help text
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Direct,
    Tar,
}

fn main() {
    match run(env::args_os()) {
        Ok(()) => {}
        Err(CliExit::Success(message)) => {
            print!("{message}");
        }
        Err(CliExit::Failure(message)) => {
            eprintln!("{message}");
            process::exit(1);
        }
    }
}

enum CliExit {
    Success(String),
    Failure(String),
}

fn run<I>(args: I) -> Result<(), CliExit>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();

    let mut mode = Mode::Auto;
    let mut path: Option<OsString> = None;

    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Err(CliExit::Success(HELP.to_string()));
        }
        if let Some(value) = arg.to_str().and_then(|value| value.strip_prefix("--mode=")) {
            mode = parse_mode(value)?;
            continue;
        }
        if arg == "--mode" {
            let Some(value) = args.next() else {
                return Err(usage_error("missing value for --mode"));
            };
            mode = parse_mode(&value.to_string_lossy())?;
            continue;
        }
        if arg.to_string_lossy().starts_with('-') && arg != "-" {
            return Err(usage_error(&format!(
                "unknown flag '{}'",
                arg.to_string_lossy()
            )));
        }
        if path.is_some() {
            return Err(usage_error("unexpected extra positional argument"));
        }
        path = Some(arg);
    }

    let Some(path) = path else {
        return Err(usage_error("missing path argument"));
    };

    let hash = match (mode, path.as_os_str() == "-") {
        (Mode::Tar, true) => hash_tar_reader(io::stdin().lock())
            .map_err(|error| CliExit::Failure(error.to_string()))?,
        (Mode::Auto | Mode::Direct, true) => {
            return Err(CliExit::Failure(
                "stdin is supported only with '--mode=tar'".to_string(),
            ));
        }
        (mode, false) => {
            let path = PathBuf::from(path);
            match resolve_mode(mode, &path)? {
                Mode::Direct => {
                    hash_path(&path).map_err(|error| CliExit::Failure(error.to_string()))?
                }
                Mode::Tar => {
                    hash_tar_file(&path).map_err(|error| CliExit::Failure(error.to_string()))?
                }
                Mode::Auto => unreachable!("auto mode must be resolved before hashing"),
            }
        }
    };

    println!("{hash}");
    Ok(())
}

fn parse_mode(raw: &str) -> Result<Mode, CliExit> {
    match raw {
        "auto" => Ok(Mode::Auto),
        "direct" => Ok(Mode::Direct),
        "tar" => Ok(Mode::Tar),
        _ => Err(usage_error(&format!("invalid mode '{raw}'"))),
    }
}

fn resolve_mode(mode: Mode, path: &Path) -> Result<Mode, CliExit> {
    match mode {
        Mode::Direct | Mode::Tar => Ok(mode),
        Mode::Auto => {
            if path.is_dir() {
                return Ok(Mode::Direct);
            }
            if is_tar_path(path) {
                return Ok(Mode::Tar);
            }
            Ok(Mode::Direct)
        }
    }
}

fn is_tar_path(path: &Path) -> bool {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if !metadata.is_file() {
        return false;
    }
    path.file_name()
        .map(|name| name.to_string_lossy().ends_with(".tar"))
        .unwrap_or(false)
}

fn usage_error(message: &str) -> CliExit {
    CliExit::Failure(format!("{message}\n{USAGE}"))
}
