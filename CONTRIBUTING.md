# Contributing

Short developer notes. `bobr` is Linux-only — every crate root carries a
non-Linux `compile_error!` guard, so it will not build on other platforms.

## Before committing

Run all three from the workspace root:

```sh
cargo build ; cargo build-sandbox-launcher
cargo test --workspace --all-features   # all tests, incl. feature-gated
cargo clippy --workspace --all-targets  # lints
cargo fmt --check                       # formatting
cargo doc --workspace --no-deps         # doc links (broken links are denied)
```

### Running tests

- `cargo test --workspace` runs unit tests, the un-gated integration tests, and
  doc tests across every crate.
- `--all-features` additionally turns on the `integration-tests` feature, which
  gates **environment-dependent** tests (they drive the real sandbox:
  namespaces, mounts). Today that is `tree_directory_recipe_*` and
  `tree_symlink_recipe_*`.
- Those gated tests need a suitable Linux environment. Where only `podman` may
  create user namespaces (e.g. an AppArmor-restricted host), run them under
  `podman unshare`.
- Prefer `--all-features` over `--all-targets` for "everything": `--all-targets`
  adds examples/benches but **drops doc tests**.

To list the features that exist:

```sh
grep -rn "^\[features\]" --include=Cargo.toml .
```

### Lints

Lint levels are set centrally in the root `Cargo.toml` (`[workspace.lints]`) and
inherited by each crate via `[lints] workspace = true`. `missing_docs` is
currently `warn` while the API docs are being filled in;
`rustdoc::broken_intra_doc_links` is `deny`, so `cargo doc` fails on a broken
intra-doc link.
