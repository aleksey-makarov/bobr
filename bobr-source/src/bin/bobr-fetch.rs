//! Downloads a fetch request's sources into the store; see
//! [`bobr_source::fetch`] for the whole design.

fn main() -> std::process::ExitCode {
    bobr_source::fetch::main()
}
