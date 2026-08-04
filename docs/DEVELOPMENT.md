# Development

[Getting Started](./GETTING_STARTED.md) describes using `bobr`: unpack a
release, put its `bin/` on `PATH`, build. This chapter is about working *on* it,
where the binaries come from a checkout you are editing and the recipes tree is
one you keep changing.

The arrangement is deliberately the same as a user's. You build the binaries into
a directory and keep that directory on `PATH`; from there, everything —
the recipes' `bin/bobr-build.sh`, the QEMU bundles, `bobr-rebuild-world.sh` —
finds them exactly as it finds an unpacked release. Nothing downstream knows or
cares that the binaries came from source.

```text
<workspace>/
  bobr/           # the engine, a git checkout
  bobr-recipes/   # the recipes, a git checkout
  bobr-bin/bin/   # the binaries you build, and what you put on PATH
  bobr-store/     # the store
```

Put `<workspace>/bobr-bin/bin` on `PATH` once, ahead of anything else that might
provide `bobr`, and the rest of this chapter follows.

## Installing the binaries

`tools/build-dev.sh` builds this checkout and installs it:

```sh
tools/build-dev.sh [--quick] [--debug]
```

It runs the checks you would otherwise run by hand, then installs — in this
order, so that formatting is caught in a second rather than after three minutes
of compiling, and so that a failed build never replaces working binaries:

1. `cargo fmt --all --check`
2. `cargo build --release`
3. `cargo build-sandbox-launcher-<arch>` — `bobr-sandbox-launcher`, static musl
4. `cargo build-bundle-launcher-<arch>` — `bobr-bundle-launcher`, static musl
5. `cargo clippy --workspace --all-targets`
6. `cargo test --workspace --all-features`
7. `cargo doc --workspace --no-deps`
8. install `bobr`, `fsobj-hash`, and `bobr-sandbox-launcher`

`--quick` keeps only the build and the install, for when you are iterating and
will run the checks before committing. `--debug` installs debug binaries; the
default is release, because these are what multi-hour recipe builds run on.
`BOBR_DEV_BIN` overrides the install directory.

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

One more detail: the checks run in the debug profile while the installed binaries
are release. They compile faster that way, and nothing about them depends on the
profile.

This is stricter than CI, which runs `fmt`, `clippy`, and `test` without
`--all-features` and does not build the docs. `--all-features` turns on the
`integration-tests` feature, and `cargo doc` is what catches broken intra-doc
links, which are denied workspace-wide.

## Building recipes

There is no separate build driver for development. With the binaries on `PATH`,
use `bin/bobr-build.sh` exactly as
[Getting Started](./GETTING_STARTED.md#building-a-real-target) describes it.

One difference matters while editing recipes. Local sources are pinned by a
`*.fsobj-hash` lock beside them, and the driver **checks** those locks rather
than rewriting them: a stale lock would otherwise build the old content of a file
you just edited, silently, since the hash it still declares names an object the
store already has. So after editing a patch, a build script, or anything else
under a local `Source`, refresh the locks yourself:

```sh
bin/bobr-update-fsobj-hashes.sh
```

The build tells you when this is needed, and names the tool.

## Before tagging a release

```sh
tools/release-check.sh [TAG]
```

`TAG` defaults to `v` plus the workspace version, and giving a different one is
refused — the release workflow reads the version the same way and rejects a tag
that disagrees, so it is worth learning here rather than from a job that has
already started.

It then runs what that workflow runs, short of publishing: the formatting,
clippy and test pass, then a static musl build and the real packaging script for
both archives. That last part is the point. The packaging script writes a
request by hand, checks the sandbox launcher's protocol version and verifies
static linkage, and none of it is reached by `cargo test` or by CI on master —
so it can rot unnoticed until a tag is pushed, which is exactly how a request
schema bump once broke a release.

The aarch64 half stays with CI. The build is skipped unless that target is
installed, and the launcher tests need a machine of that architecture to run on
at all.

## Rebuilding the world

`tools/dev/bobr-rebuild-world.sh`, in the recipes repository, rebuilds
everything from scratch, into a store that has never been written to. Use it to
prove a build works from nothing — a cached store can hide a recipe that no
longer builds, because the object it would produce is already there.

```sh
tools/dev/bobr-rebuild-world.sh [--no-pull] [--jobs N] [TARGET]
```

`TARGET` defaults to `test_all`. In order, the script:

1. pulls both repositories (`--no-pull` builds what is checked out instead);
2. installs the binaries through the engine's `tools/build-dev.sh --quick`;
3. creates `<workspace>/bobr-store.<YYMMDDhhmmss>` and writes a build profile
   inside it naming that store and the target;
4. **seeds source objects** from the previous store by hardlink, so the same
   tarballs are not downloaded again — sources are content-addressed, so a
   hardlink is as good as a fetch;
5. refreshes the hash locks, then builds through `bin/bobr-build.sh`;
6. repoints the `bobr-store` symlink at the new store — **only if the build
   succeeded**, so a failed rebuild leaves you with the last good one.

Beside the store it records what produced it: `hashes.txt` with both commits,
`request.json` with the lowered request, `bobr-rebuild-world.log` with the
per-phase timings, and `host-stats.log` with load and memory samples taken
around each phase.

Expect hours. Recorded runs took 76 and 106 minutes with the sources already
present; from an empty workspace the downloads add to that. Old stores are left
alone — remove them when you are sure you no longer want to fall back to one.
