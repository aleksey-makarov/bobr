# Getting Started

Recipes and build profiles are written in [Nickel](https://nickel-lang.org/).
Basic familiarity with the language will help you follow the examples below.

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
    import "bobr-recipes/profiles/provider-public.ncl",
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

## Boot QEMU from a host bundle

You can also run programs built by Bobr on your host using a host bundle, which
packages a program with its runtime dependencies. Host bundles are still a proof
of concept, but `host_bundle_qemu` includes QEMU and the boot images, so no host
QEMU installation is needed. Access to `/dev/kvm` is still required.

Build and run directly from the store:

```sh
bobr-recipes/bin/bobr-build.sh --target host_bundle_qemu
bobr-store/object-refs/host-bundle-qemu/bin/bobr-run-qemu
```

The runner creates a persistent `home.img` in the current directory. Use `--help`
for options.

`host_bundle_qemu_gnome` is also available, but it is a large download and requires
a working Vulkan driver on the host.

## Build a specific target

List the available targets:

```sh
bobr-recipes/bin/bobr-list-pkgs.sh
```

Build `gzip` using your existing profile:

```sh
bobr-recipes/bin/bobr-build.sh --target gzip
```

Inspect its filesystem-tree manifest:

```sh
cat bobr-store/object-refs/gzip-1.14
```

The list shows the attribute used by `--target` and the recipe name used in
`object-refs/`.

The manifest lists paths, file metadata, and content hashes. Regular file contents
are stored separately in `bobr-store/fs-files/`, shared between trees with
identical files. The manifest itself does not contain those bytes.

For details, see [Filesystem trees](./FS_TREE.md).

## Reuse results from another local store

After building `gzip`, create `reuse.ncl` beside `bobr.ncl`:

```nickel
let bobrpkgs = import "bobr-recipes/bobrpkgs.ncl" in
let pkgs = bobrpkgs [] in
(import "bobr-recipes/profiles/bobr-user.ncl") & {
  include pkgs,
  goals = [pkgs.gzip],
  store = "bobr-store.new",
  secondaries.providers = [
    {
      name = "previous",
      mappings = true,
      content = true,
      local = {
        store = "bobr-store",
        transfer = "hardlink",
      },
    },
  ],
}
```

Build into a new store using this profile:

```sh
mkdir bobr-store.new
bobr-recipes/bin/bobr-build.sh reuse.ncl
```

Bobr finds the build mapping in the previous store and imports the result using
hardlinks, without downloading or rebuilding. Both stores must be on the same
filesystem.

The result is available through `bobr-store.new/object-refs/`. Your original
`bobr.ncl` remains unchanged.

## Add a recipe with an overlay

An overlay adds or changes recipes without editing `bobr-recipes`, following the
same model as Nix overlays. The package set is defined as a fixed point: recipes
refer to the final set, including all overlay changes. Each overlay receives
`final`, that final set, and `prev`, the set before this overlay, and returns the
fields to add or replace.

Create `overlay.ncl` beside `bobr.ncl`:

```nickel
fun final => fun prev => {
  greeting = {
    name = "greeting",
    tag = "Tree",
    config.tree.entries = [
      {
        type = "file",
        path = "etc/greeting.txt",
        text = "Hello from my overlay!\n",
        executable = false,
      },
    ],
    inputs = {},
  },
}
```

`tag` selects the builder; [`Tree` in Request](./REQUEST.md#tree) describes how
this builder creates files and directories from inline data.
The nested path makes the result a directory containing `etc/greeting.txt`.

In `bobr.ncl`, replace:

```nickel
let pkgs = bobrpkgs [] in
```

with:

```nickel
let pkgs = bobrpkgs [import "./overlay.ncl"] in
```

Build the new target and read the resulting file:

```sh
bobr-recipes/bin/bobr-build.sh --target greeting
cat bobr-store/object-refs/greeting/etc/greeting.txt
```

Change `text` in the overlay and build again to update the file.

## Import a file into an fs-tree

Add another recipe inside the record returned by `overlay.ncl`, alongside
`greeting`:

```nickel
greeting = {
  # Keep the greeting recipe from the previous example.
},
greeting_tree = {
  name = "greeting-tree",
  tag = "FsTreeImport",
  config.install.rules = [
    {
      path = "**",
      attrs = {
        uid = 0,
        gid = 0,
        directory_mode = 0o755,
        regular_file_mode = 0o644,
        executable_file_mode = 0o755,
      },
    },
  ],
  inputs = { input = final.greeting },
},
```

[`FsTreeImport`](./REQUEST.md#fstreeimport) imports the directory into an fs-tree,
assigning root ownership and file mode `0644`. `final.greeting` refers to the
recipe from the final overlaid set.

Build and inspect the manifest:

```sh
bobr-recipes/bin/bobr-build.sh --target greeting_tree
cat bobr-store/object-refs/greeting-tree
```

The manifest records the `etc/greeting.txt` path, ownership, mode, and fs-file hash.
The file contents are stored separately in `bobr-store/fs-files/`.

The files in `fs-files/` already carry their required ownership and permissions.
Bobr can assemble a usable filesystem by hardlinking them into the layout
described by an fs-tree manifest.

## Compile a local program

Create `hello-src/` beside `bobr.ncl`, containing these two files.

`hello.c`:

```c
#include <stdio.h>

int main(void)
{
    puts("Hello from Bobr!");
    return 0;
}
```

`Makefile` (indent commands with tabs):

```make
hello: hello.c
	$(CC) $(CFLAGS) -o $@ $<

install: hello
	mkdir -p /usr/bin
	install -m 0755 hello /usr/bin/hello

.PHONY: install
```

Record the source directory’s absolute path and content hash:

```sh
realpath hello-src > hello-source.path
bobr-fsobj-hash hello-src > hello-source.fsobj-hash
```

Create `hello.ncl` beside `bobr.ncl`:

```nickel
let hello_source = {
  name = "hello-source",
  tag = "Source",
  object_hash =
    std.string.trim (import "./hello-source.fsobj-hash" as 'Text),
  origin = {
    tag = "Path",
    path = std.string.trim (import "./hello-source.path" as 'Text),
  },
}
in
fun pkgs => {
  name = "hello",
  tag = "Makefile",
  deps = {
    build = [],
    runtime = [pkgs.c_runtime],
  },
  config = {},
  inputs.source = hello_source,
}
```

In `bobr.ncl`, replace the `goals` line with:

```nickel
goals = [(import "./hello.ncl") pkgs],
```

Goals can refer directly to recipes outside the package set; no overlay is
needed to build this recipe.

Build, inspect the stored sources, and read the resulting fs-tree manifest:

```sh
bobr-recipes/bin/bobr-build.sh
ls bobr-store/object-refs/hello-source/
cat bobr-store/object-refs/hello
```

`hello-source` points to the source directory stored by Bobr; `hello` points to
the resulting fs-tree manifest.

The first compilation requires a substantial download of the build environment.
The recipe runs `make` and `make install`, producing an fs-tree containing
`/usr/bin/hello`.

After changing either source file, update the source hash before rebuilding:

```sh
bobr-fsobj-hash hello-src > hello-source.fsobj-hash
```

## Add a library dependency

A recipe’s result contains only the files installed by that recipe. Bobr does
not inspect compiled programs to infer their dependencies. `deps.build` adds
tools, headers, and libraries to the build environment; `deps.runtime` declares
what must accompany the result when it is included in a root filesystem.

Make `hello` use zlib by replacing `hello.c` with:

```c
#include <stdio.h>
#include <zlib.h>

int main(void)
{
    printf("Hello from Bobr with zlib %s!\n", zlibVersion());
    return 0;
}
```

Add `-lz` to the link command in `Makefile`:

```make
	$(CC) $(CFLAGS) -o $@ $< -lz
```

Then change `deps` in `hello.ncl`:

```nickel
deps = {
  build = [pkgs.zlib],
  runtime = [pkgs.c_runtime, pkgs.zlib],
},
```

`pkgs.zlib` in `build` provides `zlib.h` and the library used by the linker.
The entries in `runtime` cause the C runtime and zlib to accompany `hello` when
the next section adds it to the QEMU root filesystem.

Update the source hash and rebuild:

```sh
bobr-fsobj-hash hello-src > hello-source.fsobj-hash
bobr-recipes/bin/bobr-build.sh
```

## Add the program to a QEMU system

Add these fields to the record returned by `overlay.ncl`:

```nickel
greeting = {
  # Keep the greeting recipe from the previous example.
},
greeting_tree = {
  # Keep the greeting_tree recipe from the previous example.
},
hello = (import "./hello.ncl") final,
qemu_rootfs = prev.qemu_rootfs & {
  inputs.hello = final.hello,
},
```

This adds `hello` and its runtime dependencies to the image’s root filesystem.

Rebuild the image and boot:

```sh
bobr-recipes/bin/bobr-build.sh --target qemu_image
bobr-recipes/tools/bobr-run-qemu.sh
```

Allow several minutes (about six in a test run): changing the rootfs requires
downloading its constituent objects and repacking the EROFS image. In the guest,
run:

```sh
hello
```

It prints `Hello from Bobr!`.

## Rebuild world using repository sources

Use a new working directory containing `bobr-recipes`, or remove the existing
`bobr-store` and `bobr.ncl`. Removing the store deletes all previous build results.

Create `bobr.ncl`:

```nickel
let bobrpkgs = import "bobr-recipes/bobrpkgs.ncl" in
let pkgs = bobrpkgs [] in
(import "bobr-recipes/profiles/bobr-user.ncl") & {
  include pkgs,
  goals = [pkgs.world],
  secondaries.providers = [
    (import "bobr-recipes/profiles/provider-public.ncl") & {
      mappings = false,
    },
  ],
}
```

The repository supplies content whose hash is already known, such as source
archives. Build-result mappings are disabled, so Bobr builds from source rather
than downloading prebuilt results. Missing sources are fetched from their
configured origins.

This is useful if you trust the source hashes in the recipes but do not want to
trust prebuilt Bobr results. The build still relies on a prebuilt bootstrap
environment. You can independently verify those source hashes
by downloading the sources from upstream and running
`bobr-fsobj-hash --mode=direct`. For non-executable files, this produces the same
hash as `sha256sum`.

```sh
mkdir bobr-store
bobr-recipes/bin/bobr-build.sh
```

Expect several gigabytes of downloads and many hours of compilation. `world`
builds all shipped images and host bundles.

## Build world without secondary stores

Again, use a new working directory containing `bobr-recipes`, or remove the
existing `bobr-store` and `bobr.ncl`.

Create `bobr.ncl`:

```nickel
let bobrpkgs = import "bobr-recipes/bobrpkgs.ncl" in
let pkgs = bobrpkgs [] in
(import "bobr-recipes/profiles/bobr-user.ncl") & {
  include pkgs,
  goals = [pkgs.world],
  secondaries.providers = [],
}
```

Sources come from upstream servers or local recipe files.

This bypasses the prebuilt repository. You can also build Bobr itself from source
instead of using its prebuilt binaries; see [Development](./DEVELOPMENT.md).

```sh
mkdir bobr-store
bobr-recipes/bin/bobr-build.sh
```

Expect several gigabytes of downloads and many hours of compilation. Subsequent
runs reuse results in this store.

## Next steps

- [Concepts](./CONCEPTS.md) — content addressing, objects, keys, and recipes.
- [Request](./REQUEST.md) — the request format and the built-in builders.
- [Recipes in Nickel](./NICKEL.md) — authoring recipes.
- [HostBundle](./HOST_BUNDLE.md) — relocatable host-side application bundles.
- [Store](./STORE.md) — how results are stored, named, and reused.
