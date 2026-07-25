//! Command-line entry point for `bobr-bundle-launcher`.

#[cfg(not(target_os = "linux"))]
compile_error!("bobr requires Linux");

use bobr_bundle_launcher::locate_current_bundle;

fn main() {
    match locate_current_bundle() {
        Ok(location) => {
            eprintln!(
                "bobr-bundle-launcher: launching is not implemented yet; bundle root is '{}'",
                location.root().display()
            );
        }
        Err(error) => {
            eprintln!("bobr-bundle-launcher: {error}");
        }
    }
    std::process::exit(2);
}
