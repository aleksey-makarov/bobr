# Getting Started

This chapter goes from a downloaded `bobr` release to a built object twice:
first by running `bobr` on a tiny request by hand, then by building a real
target from the Nickel recipes. For the ideas behind it all, see
[Concepts](./CONCEPTS.md).

## Prerequisites

- An x86-64 Linux host. The main release archive is currently published for
  `x86_64-unknown-linux-musl`.
- `curl`, `tar`, and `sha256sum` to download, unpack, and verify the release.
- `newuidmap` and `newgidmap` on `PATH` (the `shadow` / `uidmap` package). Bobr
  runs each builder in a Linux user namespace when you are not root, and uses
  these setuid helpers to set up the uid/gid map. As root — or under `podman
  unshare` — bobr uses its in-process host runtime and needs neither.
- `nickel` — for the recipe workflow below.

## Install bobr

Choose a release from the
[bobr releases](https://github.com/aleksey-makarov/bobr/releases), download the
main archive and its checksum file, and verify it before unpacking. For example,
for version 0.1.5:

```sh
BOBR_VERSION=0.1.5
BOBR_TARGET=x86_64-unknown-linux-musl
BOBR_ARCHIVE="bobr-v${BOBR_VERSION}-${BOBR_TARGET}.tar.xz"
BOBR_RELEASE="https://github.com/aleksey-makarov/bobr/releases/download/v${BOBR_VERSION}"

curl -fLO "${BOBR_RELEASE}/${BOBR_ARCHIVE}"
curl -fLO "${BOBR_RELEASE}/SHA256SUMS"
sha256sum --ignore-missing --check SHA256SUMS
tar -xf "${BOBR_ARCHIVE}"
export PATH="${PWD}/bobr-v${BOBR_VERSION}-${BOBR_TARGET}/bin:${PATH}"
```

The archive contains static `bobr`, `fsobj-hash`, and
`bobr-sandbox-launcher` binaries. Keep its `bin/` on `PATH` for the rest of
this chapter; `bobr` finds the sandbox launcher next to its own executable.

## Your first build

`bobr` reads a JSON [request](./REQUEST.md) — a DAG of recipes — from standard
input (or a file named on the command line), builds the `root` recipe, and
prints its [`ObjectHash`](./CONCEPTS.md) to standard output.

Create the directories the request names — the store, plus a log and a work
directory for this run — and write a tiny request that stages one text file with
the [`Tree`](./REQUEST.md#tree) builder. `bobr` writes into directories you give
it and creates none of them itself, which is what lets you decide where a run's
logs and scratch go, and keeps two runs from sharing them:

```sh
mkdir -p /tmp/bobr-store /tmp/bobr-store/logs/first /tmp/bobr-store/work/first
```

`hello.json`:

```json
{
  "schema": "bobr-request-v2",
  "store": "/tmp/bobr-store",
  "logs": "/tmp/bobr-store/logs/first",
  "work": "/tmp/bobr-store/work/first",
  "run_id": "first",
  "nodes": {
    "root": {
      "name": "hello",
      "tag": "Tree",
      "config": {
        "tree": {
          "entries": [
            { "type": "file", "path": "hello.txt", "text": "hello, bobr\n", "executable": false }
          ]
        }
      },
      "inputs": {}
    }
  }
}
```

Build it:

```sh
bobr < hello.json
```

bobr prints the object's hash:

```text
f9deedd4f7809f00b0ef6fe93d4001fd70f8495478dafdaa8a01c34ef0269af1
```

The hash is derived from the content, not from the path or name, so you will get
the same value. The result is in the store:

```sh
$ cat /tmp/bobr-store/objects/f9deedd4f7809f00b0ef6fe93d4001fd70f8495478dafdaa8a01c34ef0269af1
hello, bobr
$ readlink /tmp/bobr-store/object-refs/hello
../objects/f9deedd4f7809f00b0ef6fe93d4001fd70f8495478dafdaa8a01c34ef0269af1
```

Because the single entry is one top-level file, the object is that file; a tree
with more entries would produce a filesystem-tree object instead (see
[Filesystem trees](./FS_TREE.md)). The `object-refs/hello` symlink is the
human-facing name for the result (see [Store](./STORE.md)).

## Building a real target

Writing requests by hand does not scale; real targets are authored in
[Nickel](https://nickel-lang.org/) in the separate
[**bobr-recipes**](https://github.com/aleksey-makarov/bobr-recipes) repository
and lowered to a request. The two are released together, so take the recipes
release matching the `bobr` you unpacked above:

```sh
curl -fLO "https://github.com/aleksey-makarov/bobr-recipes/archive/refs/tags/v${BOBR_VERSION}.tar.gz"
tar -xf "v${BOBR_VERSION}.tar.gz"
mv "bobr-recipes-${BOBR_VERSION}" bobr-recipes
```

Versions that disagree are caught rather than left to misbehave: the recipes
name the request format they emit, and the build driver checks it against what
`bobr --version` accepts before doing anything.

Building takes two things: a **store** to build into, and a **build profile**
describing your installation. The profile is a small Nickel file you keep in your
working directory; start from the shipped example and read what is in it:

```sh
cp bobr-recipes/bobr.ncl.example bobr.ncl
mkdir bobr-store
bobr-recipes/bin/bobr-build.sh
```

That builds the profile's `target`, which the example sets to `test_all` — every
shipped artifact plus the checks over them. Expect it to run for hours: nothing
arrives pre-built, so the first build starts at the toolchain and works its way
up. To try something smaller first, list what there is and name it:

```sh
bobr-recipes/bin/bobr-list-pkgs.sh          # attribute, recipe name, tag
bobr-recipes/bin/bobr-build.sh --target gzip
```

`bobr-list-pkgs.sh` reads the same profile, so the list already reflects any
overlays it applies.

The profile holds what does not change between builds — the store, the log and
work directories, overlays to apply, whether to run under `podman unshare`. The
few things that belong to one invocation stay on the command line:

- `--target NAME` — build this instead of the profile's target;
- `--jobs N`, `--quiet` — for this run only;
- `--dry-run` — print the resolved profile and the JSON request, build nothing;
- a positional argument names a different profile (`bobr-build.sh ../ci/bobr.ncl`).

`bobr` and `fsobj-hash` are taken from `PATH` — the ones from the release archive
you unpacked earlier. Nothing is guessed, so what gets used is what `bobr
--version` reports; the driver checks that its request format matches these
recipes before it starts, and says so plainly when it does not.

Later builds reuse cached objects and rebuild only what a change actually
reaches, so that cost is paid once rather than on every build.

The result is referenced at `object-refs/<name>`, but do not expect a directory
of installed files there: for a package like `gzip` that object is an
[fs-tree](./FS_TREE.md) **manifest** — a small text file listing each entry and,
for regular files, its content hash. The bytes themselves live deduplicated in
`fs-files/`, each stored with its correct (in-namespace) owner, group, and mode.
This manifest + `fs-files/` split exists so a tree can be reproduced *quickly*
and with the exact per-file ownership and mode a container's root filesystem
needs — which a plain single-owner store directory could never hold. When a
builder consumes the fs-tree it is **materialized** into a real directory under
`fs-trees/<hash>/` (files hardlinked from `fs-files/`), and `fs-tree-refs/<name>`
is a by-name symlink to that materialized root; that tree is what gets
bind-mounted as a container root. To pull specific files out as ordinary files,
use an [`FsTreeExport`](./REQUEST.md) recipe.

To author or extend recipes, see [Recipes in Nickel](./NICKEL.md).

## Changing a recipe without editing it

The recipes are a fixed-point set of plain data records, and an **overlay** is a
function over that set: given the set as it stands, it returns the fields that
should differ. The profile lists the overlays to apply, so your changes live
next to your profile rather than as edits inside the recipes checkout — which
keeps them intact when you update it.

Put this in `overlay.ncl`, beside your `bobr.ncl`:

```nickel
fun final => fun prev => {
  linux = prev.linux & {
    config_options = prev.linux.config_options & { LOCALVERSION = "-mine" },
  },
}
```

`CONFIG_LOCALVERSION` is appended to the kernel release, so this is a change you
can see rather than infer. Note what is being reached into: the kernel
configuration is an ordinary record of option names, the same kind of data as a
package's version, and an overlay gets at it the same way. There is no separate
mechanism for the kernel.

Point the profile at it:

```nickel
overlays = ["./overlay.ncl"],
```

Nothing needs building to check that an overlay took effect — `bobr-list-pkgs.sh`
and `--dry-run` both apply overlays, so the lowered request already shows the
result:

```sh
bobr-recipes/bin/bobr-build.sh --target linux --dry-run | grep LOCALVERSION
```

Then build and boot (see the next section), and the guest answers for itself:

```sh
uname -r          # 6.18.38-mine, where it used to say 6.18.38-bobr
```

### Two things that catch people out

**Merging a field that already has a value fails.** Above, `config_options` is
declared with `| default`, and overriding a default is what merging is for. A
field holding a plain value is different: `&` refuses, and for arrays it says so
in terms of an equality contract, which reads like a bug in your overlay rather
than a rule. Force the field instead:

```nickel
less = prev.less & {
  config.configure_args
    | force
    = prev.less.config.configure_args @ ["--with-editor=vim"],
},
```

**A version pin needs the matching source hash.** A different version is a
different tarball, and the hash in the recipe is what proves the download is the
right one. Set `version` and `source_object_hash` together; build once with a
wrong hash on purpose, and the error reports the real one to paste in.

### What a change costs

An override rebuilds the recipe it names and everything downstream of it, so
where you aim matters. Tagging the kernel rebuilds the kernel, the initramfs,
every root filesystem and image built from them. Changing a package that only a
few things use costs a fraction of that.

Downstream is decided by the *result*, not by the edit: if a rebuilt recipe
produces the identical object — a configure flag that turns out to select what
was already the default, say — then everything depending on it sees an unchanged
input and stays cached. A build reporting one rebuilt recipe and hundreds of
cache hits is that working, not a sign the overlay was ignored.

## Booting a system under QEMU

`host_bundle_qemu` is a self-contained directory object carrying QEMU, its
userspace runtime, and the kernel, initramfs, and EROFS image it boots. Nothing
has to be installed on the host — not even QEMU. Build it like any other recipe
target:

```sh
bin/bobr-build.sh --target host_bundle_qemu
```

The result is an ordinary directory rather than an fs-tree manifest. Add its
public `bin/` to `PATH` before leaving the recipes checkout, then run from a
writable working directory:

```sh
export PATH="$(readlink -f ../bobr-store/object-refs/host-bundle-qemu)/bin:${PATH}"
mkdir -p /tmp/bobr-qemu-run
cd /tmp/bobr-qemu-run
bobr-run-qemu
```

The runner uses the QEMU and boot artifacts inside the bundle, while still
using host interfaces such as `/dev/kvm`. It boots the plain image headlessly,
puts the serial console on the terminal, creates a sparse 1 GiB `home.img` in
the working directory, creates `diag.sock`, and forwards host TCP port 2222 to
the guest's SSH port. Run it with `--help` for resource and path options; QEMU's
`Ctrl-A X` escape exits the VM.

The graphical systems come as their own bundles, built and run the same way:
`host_bundle_qemu_weston` and `host_bundle_qemu_gnome`. Those open a window and
need KVM. Each names its `home.img` and `diag.sock` after itself, so all three
can run side by side in one working directory.

The other public entries can be used directly as well:

```sh
qemu-img --help
qemu-system-x86_64 --version
```

Adding `bin/` to `PATH` only exposes the public commands; the launcher selects
the copied glibc loader, libraries, and per-command environment. See
[HostBundle](./HOST_BUNDLE.md) for the directory layout, verifier, wrappers, and
portability boundary.

## Next steps

- [Concepts](./CONCEPTS.md) — content addressing, objects, keys, and recipes.
- [Request](./REQUEST.md) — the request format and the built-in builders.
- [Recipes in Nickel](./NICKEL.md) — authoring recipes.
- [HostBundle](./HOST_BUNDLE.md) — relocatable host-side application bundles.
- [Store](./STORE.md) — how results are stored, named, and reused.
