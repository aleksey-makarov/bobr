mod helper;
mod initramfs_writer;
mod ownership;
mod tar_writer;

fn main() -> std::process::ExitCode {
    helper::main_from_env()
}
