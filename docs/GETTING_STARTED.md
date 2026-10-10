# Getting Started

## Download the recipes

In your working directory, download the
[bobr-recipes](https://github.com/aleksey-makarov/bobr-recipes) release matching
your installed Bobr:

```sh
BOBR_VERSION="$(bobr --version | awk '{print $2}')"
curl -fLO "https://github.com/aleksey-makarov/bobr-recipes/archive/refs/tags/v${BOBR_VERSION}.tar.gz"
tar -xf "v${BOBR_VERSION}.tar.gz"
mv "bobr-recipes-${BOBR_VERSION}" bobr-recipes
```

## Build and boot a QEMU system

Create `bobr.ncl` beside `bobr-recipes` with this content:

```nickel
let bobrpkgs = import "bobr-recipes/bobrpkgs.ncl" in
let pkgs = bobrpkgs [] in
(import "bobr-recipes/profiles/bobr-user.ncl") & {
  include pkgs,
  goals = [pkgs.qemu_image, pkgs.initrd, pkgs.linux_bzimage],
  secondaries.providers = [
    import "bobr-recipes/profiles/provider-hetzner.ncl",
  ],
}
```

`bobr.ncl` is your build profile. `bobr-build.sh` evaluates it with Nickel and
generates a JSON request for Bobr. This profile imports the package set and user
defaults, selects three boot artifacts as goals, and enables a repository of
prebuilt objects to speed up the build.

```sh
mkdir -p bobr-store
bobr-recipes/bin/bobr-build.sh
```

This produces the EROFS root filesystem image, initramfs, and kernel.
You can find them through the `qemu-image`, `initrd`, and `linux-bzimage` links
in `bobr-store/object-refs/`. Allow roughly 1–2 minutes to download the prebuilt
objects, depending on your connection.

To run the script, `qemu-system-x86_64` and `mkfs.ext4` must be on `PATH`, and
your user must have access to `/dev/kvm` on an x86-64 Linux host:

```sh
bobr-recipes/tools/bobr-run-qemu.sh
```

The script finds the boot artifacts in the adjacent `bobr-store` and creates
`bobr-home.img` beside it on first launch: a persistent 1 GiB ext4 disk for
`/home`. It reuses that disk on subsequent launches.

The terminal becomes the guest's root console, with automatic login. Shut down
with `poweroff`; press **Ctrl-A, X** to exit QEMU directly. Changes under `/home`
persist in `bobr-home.img`; changes elsewhere are discarded on reboot.

## Build and boot a GNOME system

Using the same profile, build the GNOME image:

```sh
bobr-recipes/bin/bobr-build.sh --target qemu_gnome_image
```

Alternatively, change the profile’s `goals` to `[pkgs.qemu_gnome_image]` and run
without `--target`.

Your host QEMU must support SDL/OpenGL and virtio GPU with Venus; the host also
needs a working Vulkan driver. See
[QEMU graphics requirements](https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html).

Launch GNOME:

```sh
bobr-recipes/tools/bobr-run-qemu-gnome.sh
```

The desktop opens in a window. The script reuses `bobr-home.img`; do not run
both guests simultaneously.

The systems booted above are still proof-of-concept demos, with much left to
improve. For now, Bobr’s primary focus is the build system rather than a complete
OS.

## Your first build

`bobr` reads a JSON [request](./REQUEST.md) — a DAG of recipes — from standard
input (or a file named on the command line), realizes its ordered goals, and
prints their [`ObjectHash`](./CONCEPTS.md) values to standard output. This
first request has one goal, named `root`.

Create the directories the request names — the store, plus a log and a work
directory for this run — and write a tiny request that stages one text file with
the [`Tree`](./REQUEST.md#tree) builder. `bobr` writes into directories you give
it and creates none of them itself, which is what lets you decide where a run's
logs and scratch go, and keeps two runs from sharing them.

The store and work directory must be on the same filesystem. The layout below
keeps them together:

```sh
mkdir -p /tmp/bobr-store /tmp/bobr-store/logs/first /tmp/bobr-store/work/first
```

`hello.json`:

```json
{
  "schema": "bobr-request-v6",
  "store": "/tmp/bobr-store",
  "logs": "/tmp/bobr-store/logs/first",
  "work": "/tmp/bobr-store/work/first",
  "run_id": "first",
  "goals": ["root"],
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
and lowered to a request.

Building takes two things: a **store** to build into, and a **build profile**
describing your installation. The profile is a small Nickel file you keep in
your working directory. Import the public package-set function and the
maintained user preset, then select the ordered goals explicitly:

```sh
cat > bobr.ncl <<'EOF'
let bobrpkgs = import "bobr-recipes/bobrpkgs.ncl" in
let pkgs = bobrpkgs [] in
(import "bobr-recipes/profiles/bobr-user.ncl") & {
  include pkgs,
  goals = [pkgs.world],
  secondaries.providers = [
    import "bobr-recipes/profiles/provider-hetzner.ncl",
  ],
}
EOF
mkdir bobr-store
bobr-recipes/bin/bobr-build.sh
```

One command realizes the complete target. `bobr` checks the working and
configured secondary stores, acquires missing Source content from local paths,
HTTP mirrors, or OCI registries, and runs builders only where cache resolution
misses.

That builds the profile's `world` goal — every shipped OS image and HostBundle,
without the separate acceptance tests. The Hetzner repository supplies matching
prebuilt results; missing results are built locally, which can take hours.
Run `--target test_all` when you want the rootfs,
HostBundle, and final-toolchain checks. To try something smaller first, list
what there is and name it:

```sh
bobr-recipes/bin/bobr-list-pkgs.sh          # attribute, recipe name, tag
bobr-recipes/bin/bobr-build.sh --target gzip
```

`bobr-list-pkgs.sh` reads the same final `pkgs`, so the list already reflects
any overlays applied while constructing it.

The profile holds what does not change between builds — the package set,
ordered goals, store, log and work directories, and whether to run under
`podman unshare`. The
few things that belong to one invocation stay on the command line:

- `--target NAME` — replace the profile goals for this run; repeat it to select
  several goals in order;
- `--jobs N`, `--quiet` — for this run only;
- `--dry-run` — print the resolved profile and the JSON request, build nothing;
- a positional argument names a different profile (`bobr-build.sh ../ci/bobr.ncl`).

`bobr` and `bobr-fsobj-hash` are taken from `PATH` — normally the ones the
installer placed there. Nothing is guessed, so what gets used is what
`bobr --version` reports; the driver checks `bobr --build-info` to ensure its
request format matches these recipes before it starts, and says so plainly when
it does not.

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
should differ. The profile imports and applies overlays while constructing
`pkgs`, so your changes live next to your profile rather than as edits inside
the recipes checkout — which keeps them intact when you update it.

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

Apply it when constructing the package set:

```nickel
let bobrpkgs = import "bobr-recipes/bobrpkgs.ncl" in
let local_overlay = import "./overlay.ncl" in
let pkgs = bobrpkgs [local_overlay] in
(import "bobr-recipes/profiles/bobr-user.ncl") & {
  include pkgs,
  goals = [pkgs.world],
  secondaries.providers = [
    import "bobr-recipes/profiles/provider-hetzner.ncl",
  ],
}
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
bobr-recipes/bin/bobr-build.sh --target host_bundle_qemu
```

The result is an ordinary directory rather than an fs-tree manifest. Resolve
its path while still in the directory containing your profile, add its public
`bin/` to `PATH`, then run from a writable working directory:

```sh
bundle="$(readlink -f bobr-store/object-refs/host-bundle-qemu)"
export PATH="${bundle}/bin:${PATH}"
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
