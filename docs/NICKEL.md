# Recipes in Nickel

> **Work in progress.** This describes how Nickel recipes lower to the JSON
> request; it is being reorganized and is not yet complete.

Nickel recipes may use higher-level synthetic tags that are lowered before
`bobr` sees the request.

## Package-aware helpers

`Autotools`, `Makefile`, `Meson`, `PerlModule`, and `SandboxBuild` lower to
Rust-side `Sandbox` nodes. They require `deps = { build = [...], runtime = [...] }`
and do not require or consume `inputs.rootfs`: the lowering layer builds a
temporary `TreeMerge` rootfs from `base_filesystem`, the runtime closure of the
helper's default build tools, and the runtime closure of `deps.build`, then
injects that rootfs into the lowered `Sandbox` request. The published package
runtime dependencies remain the recipe's `deps.runtime`.

Default build tools:

- `Autotools`: the common native toolchain plus `autoconf`, `m4`, and `perl`
- `Makefile`: the common native toolchain
- `Meson`: the common native toolchain plus `pkgconf` and `python`
- `PerlModule`: the common native toolchain plus `perl`
- `SandboxBuild`: `bash`, `tar`, `gzip`, `bzip2`, `xz`, and `patch`

The common native toolchain is `linux_headers`, `glibc`, `binutils`, `gcc`,
`bash`, `make`, `coreutils`, `gawk`, `sed`, `grep`, `tar`, `gzip`, `xz`,
`bzip2`, `patch`, `findutils`, and `diffutils`.

## Explicit-rootfs helpers

`AutotoolsRootfs`, `MakefileRootfs`, `MesonRootfs`, `PerlModuleRootfs`, and
`SandboxBuildRootfs` require `inputs.rootfs` and use it as supplied. They remain
available for bootstrap recipes and other cases where the caller must choose the
execution rootfs directly.
