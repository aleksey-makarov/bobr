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
- `git` and `nickel` — only for the recipe workflow below.

## Install bobr

Choose a release from the
[bobr releases](https://github.com/aleksey-makarov/bobr/releases), download the
main archive and its checksum file, and verify it before unpacking. For example,
for version 0.1.4:

```sh
BOBR_VERSION=0.1.4
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

Create a store (an absolute path to an existing directory) and a tiny request
that stages one text file with the [`Tree`](./REQUEST.md#tree) builder:

```sh
mkdir -p /tmp/bobr-store
```

`hello.json`:

```json
{
  "schema": "bobr-request-v1",
  "store": "/tmp/bobr-store",
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
and lowered to a request. Clone it into the current working directory:

```sh
git clone https://github.com/aleksey-makarov/bobr-recipes
```

List the build targets — attribute names with their package names — with
`tools/bobr-list-pkgs.sh`, then build one with the driver, which refreshes the
local `fsobj-hash` locks with the binary from the release archive, exports the
request through `request.ncl`, and runs `bobr`:

```sh
cd bobr-recipes
tools/bobr-list-pkgs.sh              # choose a target attribute
tools/bobr-build.sh gzip            # build one package
tools/bobr-build.sh test_all_recipes  # or the whole shipped set: kernel, images, initrd
```

`tools/bobr-build.sh` options:

- `--store PATH` — where to build (default: `../bobr-store`, next to the repos);
- `--jobs N`, `--quiet` — the request's top-level knobs;
- `--podman-unshare` — run under `podman unshare` on hosts that forbid
  unprivileged user namespaces.

The first real build bootstraps a toolchain from source (glibc, gcc, …), so it
takes a while; later builds reuse cached objects and rebuild only what changed.

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

## Rebuilding the world

`tools/bobr-rebuild-world.sh [attr]` builds into a fresh, timestamped store
(`bobr-store.<timestamp>`, with the `bobr-store` symlink repointed at it), seeds
source objects from the previous store by hardlink, records the `bobr` and
`bobr-recipes` commits, and runs the build through `bobr-build.sh`.

## Booting a system under QEMU

`tools/bobr-run-qemu-gnome.sh` is a quick smoke test: it builds the kernel, the
GNOME EROFS rootfs, and initrd (through `bobr-build.sh`) and boots them under the
host's `qemu-system-x86_64` in a graphical window. It needs KVM (`/dev/kvm`);
`--store PATH` selects the store (default `../bobr-store`), and anything after
`--` is passed through to QEMU.

```sh
tools/bobr-run-qemu-gnome.sh
```

## Running QEMU from a HostBundle

The previous helper uses `qemu-system-x86_64` installed on the host. For
contrast, `host_bundle_qemu` is a self-contained directory object carrying
QEMU, its userspace runtime, and the kernel, initramfs, and EROFS image it
boots. Build it like any other recipe target:

```sh
tools/bobr-build.sh host_bundle_qemu
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

The other public entries can be used directly as well:

```sh
qemu-img --help
qemu-system-x86_64 --version
```

No QEMU installation is needed on the host. Adding `bin/` to `PATH` only
exposes the public commands; the launcher selects the copied glibc loader,
libraries, and per-command environment. See [HostBundle](./HOST_BUNDLE.md) for
the directory layout, verifier, wrappers, and portability boundary.

## Next steps

- [Concepts](./CONCEPTS.md) — content addressing, objects, keys, and recipes.
- [Request](./REQUEST.md) — the request format and the built-in builders.
- [Recipes in Nickel](./NICKEL.md) — authoring recipes.
- [HostBundle](./HOST_BUNDLE.md) — relocatable host-side application bundles.
- [Store](./STORE.md) — how results are stored, named, and reused.
