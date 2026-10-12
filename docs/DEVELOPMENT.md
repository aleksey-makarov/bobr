# Development

Commands below assume sibling `bobr` and `bobr-recipes` checkouts and are run
from their parent directory. Recipe builds require the environment described
in [Installation](./INSTALLATION.md#prerequisites).

## Build and check Bobr

Source builds require Rust/Cargo, a native C toolchain, and `rustfmt`/`clippy`.
The complete release gate also requires the `x86_64-unknown-linux-musl` Rust
target, `musl-gcc`, and packaging tools including `readelf`, `strip`, `tar`,
`xz`, and `sha256sum`. It runs on x86-64 Linux and requires the exact Rust
version selected by `RUST_TOOLCHAIN` in both CI and release workflows.

For an edit-compile-test cycle:

```sh
(cd bobr && cargo build --workspace --locked)
(cd bobr && cargo test --workspace --locked)
```

Additional tests gated by `integration-tests` require a working Linux sandbox
environment (user namespaces and mounts):

```sh
(cd bobr && cargo test --workspace --all-features --locked)
```

CI and the release gate do not currently enable these feature-gated tests.

These commands do not install binaries. Before acceptance testing, run the
complete release gate and install its verified archive:

```sh
bobr/tools/release-check-and-install.sh
```

The gate checks formatting, clippy, tests, rustdoc, the installer, and static
musl release packaging and smoke tests. It installs `bobr`, `bobr-repo`,
`bobr-fsobj-hash`, and `bobr-sandbox-launcher` only after all checks pass.
The bundle launcher is checked but not installed.

A clean checkout is required by default. Use `--allow-dirty` while developing;
the installed `bobr --build-info` records the commit and dirty state.
The destination is `$CARGO_INSTALL_ROOT/bin`, then `$CARGO_HOME/bin`, then
`~/.cargo/bin`; `--bin-dir DIR` overrides it. Put that directory before other
Bobr installations on `PATH`.

## Check recipes

Run the standalone recipe check suite with Nickel, `bobr-fsobj-hash`, and the
other tools required by `tools/check.sh` on `PATH`:

```sh
bobr-recipes/tools/check.sh
```

After editing local sources in the recipes checkout, refresh their adjacent
`*.fsobj-hash` locks:

```sh
bobr-recipes/bin/bobr-update-fsobj-hashes.sh
```

`bobr-build.sh` checks existing locks in that checkout; it does not rewrite
them or check external user sources automatically. Validate a profile and
lower its request without building:

```sh
bobr-recipes/bin/bobr-build.sh --dry-run /path/to/profile.ncl
```

For profile and target selection, see
[Getting Started](./GETTING_STARTED.md#build-a-specific-target).

## Acceptance builds

For a cold `world` build, create an empty store and set the profile's `store`
to that path. Disable mappings on every secondary provider, or use
`secondaries.providers = []`; an empty working store alone does not prevent
prebuilt mapping hits. Content-only local or remote providers are allowed.
Leave `output_repository` unset unless publication is intended.

```sh
mkdir /path/to/new-store
bobr-recipes/bin/bobr-build.sh --target world /path/to/profile.ncl
bobr-recipes/bin/bobr-build.sh --target test_all /path/to/profile.ncl
```

`world` builds the shipped images and host bundles; `test_all` runs the separate
acceptance recipes and can reuse results from the preceding build. A cold
`world` build takes hours. See [Build logging](./LOGGING.md) for diagnostics.

## Documentation

Build or preview the user documentation with mdBook:

```sh
(cd bobr && mdbook build)
(cd bobr && mdbook serve)
```
