# Development

[Getting Started](./GETTING_STARTED.md) describes installing and using a
release. This chapter is about working *on* `bobr`, where the binaries come
from a checkout you are editing and the recipes tree is one you keep changing.

The arrangement is deliberately the same as a user's. You install the host
tools into one directory on `PATH`; from there, everything — the recipes'
`bin/bobr-build.sh` and the QEMU bundles — finds them
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

## Cold world builds

The artifact-only `world` target and the explicit acceptance-test target
`test_all` are ordinary profile targets. To prove the shipped artifacts build
without working-store mappings, point a profile at a newly created empty store
and run the normal driver:

```sh
mkdir /path/to/new-store
bin/bobr-build.sh --target world /path/to/profile.ncl
```

The profile may still configure content-only secondary providers, including a
previous local store or a remote repository. They can avoid downloading stable
payloads again without exposing their build or reuse mappings.

Each real invocation records its Bobr and recipes provenance, outcome, and a
compact recipe-name catalog beside the structured log in
`<logs>/<run-id>/context.json` and `recipe-catalog.json`. These records are
diagnostic metadata used by store-inspection tools; the authoritative build and
reuse mappings remain in the store itself. A dry run creates no record.

The hash locks are left alone: `bin/bobr-build.sh` checks them and refuses on a
stale one, which is what should happen to a checkout that says one thing and
contains another. Expect a cold `world` build to take hours.
