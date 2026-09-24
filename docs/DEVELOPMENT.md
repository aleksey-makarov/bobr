# Development

[Getting Started](./GETTING_STARTED.md) describes installing and using a
release. This chapter is about working *on* `bobr`, where the binaries come
from a checkout you are editing and the recipes tree is one you keep changing.

The arrangement is deliberately the same as a user's. You install the host
tools into one directory on `PATH`; from there, everything — the recipes'
`bin/bobr-build.sh`, the QEMU bundles, `bobr-rebuild-world.sh` — finds them
exactly as it finds an unpacked release. Nothing downstream knows or cares that
the binaries came from source.

```text
<workspace>/
  bobr/           # the engine, a git checkout
  bobr-recipes/   # the recipes, a git checkout
  bobr-store/     # the store
```

## Installing the binaries

Run the complete release gate and install exactly the archive it verifies:

```sh
tools/release-check-and-install.sh [--allow-dirty] [--bin-dir DIR]
```

The default destination follows Cargo's convention:
`$CARGO_INSTALL_ROOT/bin`, then `$CARGO_HOME/bin`, then `~/.cargo/bin`.
`--bin-dir` overrides it. Keep the selected directory on `PATH`; Bobr does not
maintain a workspace-private binary directory or inject one into child
processes.

The gate requires a clean checkout by default. It reads the single explicit
`RUST_TOOLCHAIN` value from both CI and release workflows, requires the two to
match, and requires the local `rustc` to have that exact version. It then runs
formatting, clippy, tests, and rustdoc, builds the static musl release archives,
and smoke-tests them. Only after every check succeeds does it replace `bobr`,
`bobr-repo`, `bobr-fsobj-hash`, and `bobr-sandbox-launcher` in the destination.
`--allow-dirty` permits an iterative build while recording that fact in
`bobr --build-info`.

For a quick edit-compile cycle, use Cargo directly. This does not update the
tools on `PATH`; run the complete gate before using a build for acceptance
testing or a long recipe build.

The directory selected by the gate must precede other Bobr installations on
`PATH`. The script warns when `command -v bobr` selects another copy. To remove
an installation, delete the four commands above from that directory; the gate
does not maintain an installation database or change shell startup files.

Two different launchers are built here, and only one of them is installed:

- **The sandbox launcher, `bobr-sandbox-launcher`, is installed** next to
  `bobr`. It is the process that sets up the user namespace for every sandboxed
  build, and `bobr` finds it by looking beside its own executable — which is also
  how the release archive is laid out, so this is the same path a user takes, not
  a development special case.
- **The bundle launcher, `bobr-bundle-launcher`, is only built, never
  installed.** It belongs to built artifacts rather than to your toolchain: it is
  the small program a [HostBundle](./HOST_BUNDLE.md) carries to select its own
  loader and libraries at run time. Recipes fetch it from a published release
  (`host-bundles/bobr-bundle-launcher.ncl`), not from this tree, so building it
  here only proves it still compiles.

## Building recipes

There is no separate build driver for development. With the binaries on `PATH`,
use `bin/bobr-build.sh` exactly as
[Getting Started](./GETTING_STARTED.md#building-a-real-target) describes it.

One difference matters while editing recipes. Local sources are pinned by a
`*.fsobj-hash` lock beside them, and `bin/bobr-build.sh` **checks** those locks
rather than rewriting them: a stale lock would otherwise reuse the old content
of a file you just edited, since the hash it still declares names an object the
store may already have. After editing a patch, a build script, or anything else
under a local `Source`, refresh the locks yourself:

```sh
bin/bobr-update-fsobj-hashes.sh
```

The build tells you when this is needed, and names the tool.

## Before tagging a release

```sh
tools/release-check-and-install.sh [--allow-dirty] [--bin-dir DIR]
```

The script derives the prospective tag from the workspace version. It requires
a clean checkout by default; `--allow-dirty` records a dirty build explicitly
for local development rather than permitting it to look like a release.

It then runs what that workflow runs, short of publishing: the formatting,
clippy, test, and rustdoc passes, then a static musl build and the real packaging
script for both archives. After every check succeeds, it installs the binaries
from the verified main archive. That last part is the point. The packaging
script writes a request by hand, checks the sandbox launcher's protocol version
and verifies static linkage, and none of it is reached by `cargo test` or by CI
on master — so it can rot unnoticed until a tag is pushed, which is exactly how
a request schema bump once broke a release.

The aarch64 half stays with CI. The build is skipped unless that target is
installed, and the launcher tests need a machine of that architecture to run on
at all.

## Publishing the user installer

The installer source is the repository-root `install.sh`. Its public URL is:

```text
https://aleksey-makarov.github.io/bobr/install.sh
```

The Pages workflow will publish it by copying `install.sh` to
`book/install.sh` after `mdbook build` and deploying that directory as the
Pages artifact. This is enabled only together with a release that provides the
stable archive names consumed by the installer. CI and release workflows
continue to run their own jobs; they invoke neither `install.sh` nor the local
developer gate.

## Rebuilding the world

`tools/bobr-rebuild-world.sh`, in the recipes repository, rebuilds the complete
artifact-only `world` target from scratch, into a store that has never been
written to. Explicit acceptance tests remain in the separate `test_all` target.
Use a world rebuild to prove the shipped artifacts build from nothing — a cached
store can hide a recipe that no longer builds, because the object it would
produce is already there.

```sh
tools/bobr-rebuild-world.sh
```

Install the tools first, either with the source checkout's release gate or with
the public `install.sh`, and put their common directory on `PATH`.
`bobr-rebuild-world.sh` does not install, update, or override them. It requires
all four host commands to resolve from the same directory, rejects a `bobr`
whose build provenance is unknown, and records the complete compact
`bobr --build-info` value in the new store. Dirty developer builds are allowed
and remain identifiable there.

In order, the script:

1. pulls the recipes and verifies the already installed host tools;
2. creates `<workspace>/bobr-store.<YYMMDDhhmmss>` and writes a build profile
   there which imports `build-profile/bobr-user.ncl` and points its store at
   the new directory — a rebuild therefore follows the maintained preset
   instead of a copied template that could drift from it;
3. adds the last successful store as an untrusted hardlink local repository;
   known Source content is acquired from it lazily, but its build and reuse
   mappings are unavailable, so this remains a cold build;
4. realizes `world` through `bin/bobr-build.sh`; Source acquisition and builder
   execution share one scheduler and one request;
5. repoints the `bobr-store` symlink at the new store — **only if the build
   succeeded**, so a failed rebuild leaves you with the last good one.

The hash locks are left alone: `bin/bobr-build.sh` checks them and refuses on a
stale one, which is what should happen to a checkout that says one thing and
contains another.

Beside the store it records what produced it: `hashes.txt` with Bobr's build
information and the recipes commit, `bobr-rebuild-world.log` with the per-phase
timings, and `host-stats.log` with load and memory samples taken around the
build.

Expect hours. Recorded runs took 76 and 106 minutes with the sources already
present; from an empty workspace the downloads add to that. Old stores are left
alone — remove them when you are sure you no longer want to fall back to one.
