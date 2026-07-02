# Recipes in Nickel

Writing a [request](./REQUEST.md) by hand is tedious: a real target pulls in
hundreds of packages — each a source plus a build recipe — all wired together by
node id. Recipes are instead written in [Nickel](https://nickel-lang.org/), a
typed configuration language, and lowered to a request. The **bobr-recipes**
repository is a worked example: a Nickel program that builds a whole Linux root
filesystem from source, starting from an OCI base image, bootstrapping a
self-hosted toolchain, and composing the result into root filesystems and disk
images.

`bobr` itself never sees Nickel — it only receives the JSON request the Nickel
layer produces. This chapter describes how that layer works in bobr-recipes.

## The package set

`pkgs.ncl` exports `mkPkgs`, a function from a list of overlays to the **package
set**; `(import "pkgs.ncl") []` builds the default set. Each package module is a
function of the finished set, so packages refer to one another by name —
`pkgs.gcc`, `pkgs.system_rootfs_1`, `pkgs.base_filesystem`. The set is a **fixed
point**: every package sees the fully assembled `pkgs`, which is what lets
recipes depend on each other and lets overlays override anything.

A package module returns named recipes:

```nickel
fun pkgs =>
  let libffi_src = fun version => {
    name = "libffi-src-%{version}",
    tag = "Source",
    object_hash = "…",
    origin = { tag = "Http", url = "https://github.com/libffi/libffi/releases/download/v%{version}/libffi-%{version}.tar.gz" },
  } in
  let libffi = {
    version | default = "3.6.0",
    name = "libffi-%{version}",
    tag = "Autotools",
    deps = { build = [], runtime = [pkgs.glibc_libs] },
    config = { configure_args = ["--disable-static"] },
    inputs = { source = libffi_src version },
  } in
  { include [libffi] }
```

`request.ncl` ties it together: given a store path, the recipes-checkout path, a
target package name, and optional overlays, it selects that package from
`mkPkgs overlays` and lowers it to a full request
(`schema`, `store`, `nodes`).

## Overlays

An **overlay** is a function `fun final => fun prev => patch`, where `prev` is
the package set built so far and `final` is the finished set. Overlays are
applied in order, and each one's fields replace the matching packages — the same
idea as Nix overlays. For example, to pin a different `shadow` version:

```nickel
[ fun final => fun prev => { shadow = prev.shadow & { version = "4.18.0" } } ]
```

Because the set is a fixed point, an override is visible everywhere the package
is used, without editing the package's own module.

## Recipes are data, not functions

For overlays to work, recipes are kept as **plain data records** — never
functions. The package set is assembled by merging records (`&`), and an overlay
overrides a package by merging new fields into it. A function value cannot be
merged or patched that way, so a recipe written as a function would be opaque to
overlays. Keeping every recipe a record means any field — a version, a configure
flag, an input — can be reached and overridden from an overlay.

## Synthetic recipes

The recipe above uses the tag `Autotools`, which is **not** a `bobr`
builder — `bobr` has no such builder. It is a **synthetic recipe**: a
high-level, Nickel-only tag that stands for a common build pattern and is
*lowered* (expanded) into real builder nodes before the request reaches `bobr`.

Lowering happens in `recipe-lib.ncl`'s `to_request`, which walks the recipe
graph and looks each node's tag up in `synthetic/registry.ncl`. A node whose tag
is already a real builder — `Tree`, `TreeMerge`, `Sandbox`, `Source`, and so
on — passes through unchanged. A node with a synthetic tag is expanded into real
builder nodes: typically a `Sandbox` that runs the build script, plus a
`TreeMerge` that assembles its build rootfs. Expansion runs on the
already-overlaid package set, so overlays always patch the high-level recipe and
expansion sees the result; `to_request` then assigns node ids and emits the JSON
`nodes` map.

## Build and runtime dependencies

A dependency-aware synthetic recipe carries `deps = { build = [...], runtime =
[...] }`:

- **`build`** — packages needed to build this one. The lowering layer assembles
  the build environment as a `TreeMerge` of `base_filesystem` with the **runtime
  closure** of a default toolchain plus these build deps, and injects it as the
  `Sandbox` rootfs.
- **`runtime`** — packages this one needs at run time. They form the package's
  own runtime closure, which is followed transitively wherever the package
  appears in another recipe's `build` or `runtime` list.

So a recipe declares what a package needs, and the root filesystem to build it in
is assembled automatically from the runtime closures of its build dependencies.

## Split outputs

The unsuffixed attribute (`pkgs.glibc`) is the full install tree. You can also
add recipes for **split outputs** — suffixed attributes carrying a subset of
that tree, so a dependent pulls in only what it needs. A split output is just a
`TreeSubset` over an [fs-tree](./FS_TREE.md), so it is very cheap: it references
the same shared fs-files as the full tree, with nothing copied. The built-in
helper `recipe.split.libs` produces a `_libs` output from the shared libraries,
loader, and SONAME symlinks — `pkgs.glibc_libs` from `pkgs.glibc`. Attributes
are `snake_case`; the builder name keeps the version in `kebab-case`
(`glibc-libs-2.42`).

Depend on the narrowest output that has the files you need — `deps.runtime` on
`_libs` for a library dependency (`pkgs.glibc_libs`, not `pkgs.glibc`), the full
attribute only when the whole tree is required.

Other suffixes are reserved for the same idea and added by hand as a
`TreeSubset` when a package needs them; today only `_libs` has a helper (plus one
hand-written `systemd_dev`):

- `_dev` — headers, pkg-config/CMake metadata, and dev symlinks
- `_tools` — executable utilities
- `_static` — static libraries
- `_doc`, `_locale`, `_data` — documentation, locale data, other
  architecture-independent runtime data

Avoid `_runtime` (already the `deps.runtime` role) and `_closure` (used by
`RootfsClosure`); `_src` and `_gen1` keep their bootstrap meaning.

## Synthetic builders

A synthetic builder expands to a `Sandbox` (see [Request](./REQUEST.md)) that
unpacks the source and runs it through a build pipeline. Each comes in two
flavors:

- the dependency-aware ones (`Autotools`, `Makefile`, `Meson`, `PerlModule`,
  `SandboxBuild`) take a `source` input and a `deps` record, and build their own
  rootfs from `deps.build`;
- the explicit-rootfs variants (`AutotoolsRootfs`, `MakefileRootfs`,
  `MesonRootfs`, `PerlModuleRootfs`, `SandboxBuildRootfs`) take an explicit
  `_rootfs` input and no `deps`, for bootstrap recipes that must choose the build
  rootfs directly.

The **common native toolchain** referenced by the defaults below is
`linux_headers`, `glibc`, `binutils`, `gcc`, `bash`, `make`, `coreutils`,
`gawk`, `sed`, `grep`, `tar`, `gzip`, `bzip2`, `xz`, `patch`, `findutils`, and
`diffutils`.

### How a synthetic build runs

Except for `SandboxBuild`, the builders share one pipeline, run as `Sandbox`
steps against the build rootfs:

1. **prepare** — unpack the source and apply patches (below);
2. any **`pre_configure`** (or, for `Makefile`, **`pre_build`**) hook steps;
3. **configure** — the tool's configure step (`Makefile` has none);
4. **build** — the tool's build step, as `build-user`, parallel (`-j` = CPU
   count);
5. **install** — as **`root`**, into the output directory via `DESTDIR=@{out}`;
   the result fs-tree is that output;
6. any **`post_install`** hook steps.

**Source and patches.** The `source` input is unpacked into `@{build}/source`;
if it is a single tarball whose contents sit under one wrapping directory, that
wrapper is stripped. Any input whose name begins with `patch` is then applied
from the source root with `patch -p1`, in sorted input-name order — a `.patch`
file directly, and a directory by applying every `*.patch` it contains. So
patches are added as extra inputs:

```nickel
inputs = {
  source = foo_src,
  patch0 = foo_fix_configure,   # a .patch Source
  patch1 = foo_extra_patches,   # or a Source directory of *.patch files
}
```

**Config.** A recipe's `config` fields reach the build script through
`script_config` (materialized at `@{config}`): `configure_args`, `make_args`,
`setup_args`, and `perl_args` become the arguments of the matching command;
`env` entries are exported into every step; `source_subdir` builds a
subdirectory of the source; and `in-tree` (Autotools) builds in the source tree
instead of a separate build directory.

**Hooks.** `pre_configure` / `pre_build` / `post_install` are extra `Sandbox`
steps injected into the pipeline — a single step or an array. Each is a normal
step (`name`, `run_as`, `argv`, optional `env`, and `cwd`, which defaults to the
source directory for pre-hooks and the output or build directory for
`post_install`), so a recipe can run arbitrary commands before configuring or
after installing without leaving the synthetic builder.

### `Autotools`

- **Steps:** `./configure --prefix=/usr <configure_args>` → `make -j <make_args>`
  → `make DESTDIR=@{out} <make_args> install`.
- **Config** (all optional): `configure_args`, `make_args`, `env`, `in-tree`,
  `source_subdir`, `pre_configure`, `post_install`.
- **Default build tools:** the common native toolchain plus `autoconf`, `m4`,
  and `perl`.

### `Makefile`

- **Steps:** no configure; `make -j <make_args>` → `make DESTDIR=@{out}
  <make_args> install` (skipped when `skip_install` is set).
- **Config** (all optional): `make_args`, `env`, `source_subdir`, `pre_build`,
  `post_install`, `skip_install`.
- **Default build tools:** the common native toolchain.

### `Meson`

- **Steps:** `meson setup <build-dir> <source> --prefix=/usr <setup_args>` →
  `meson compile -j` → `DESTDIR=@{out} meson install`.
- **Config** (all optional): `setup_args`, `env`, `source_subdir`,
  `pre_configure`, `post_install`.
- **Default build tools:** the common native toolchain plus `pkgconf` and
  `python`.

### `PerlModule`

- **Steps:** `perl Makefile.PL <perl_args>` → `make -j <make_args>` → `make
  DESTDIR=@{out} <make_args> install`.
- **Config** (all optional): `perl_args`, `make_args`, `env`, `source_subdir`,
  `pre_configure`, `post_install`.
- **Default build tools:** the common native toolchain plus `perl`.

### `SandboxBuild`

Runs your own `Sandbox` step plan instead of the pipeline above — the same
config as the built-in `Sandbox` builder (`steps`, `script_config`; see
[Request](./REQUEST.md)) — but with a rootfs built automatically from `deps`.
Source unpacking and patching are not automatic here; do them in your own steps.

- **Inputs:** `source` and `deps` (the `SandboxBuildRootfs` variant is a
  `Sandbox` with an explicit `_rootfs`).
- **Default build tools:** `bash`, `tar`, `gzip`, `bzip2`, `xz`, and `patch`.

### `RootfsClosure`

Not a build: assembles a root filesystem as a `TreeMerge` of `base_filesystem`
with the runtime closure of its inputs — the standard way to turn a set of
packages into a usable rootfs.

- **Inputs:** one or more packages (the roots of the closure).
- **Config:** none.
