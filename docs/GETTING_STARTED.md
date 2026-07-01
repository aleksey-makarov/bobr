# Getting Started

This chapter goes from a fresh checkout to a built object twice: first by
running `bobr` on a tiny request by hand, then by building a real target from
the Nickel recipes. For the ideas behind it all, see [Concepts](./CONCEPTS.md).

## Prerequisites

- A stable Rust toolchain with the `x86_64-unknown-linux-musl` target added
  (`rustup target add x86_64-unknown-linux-musl`) — needed to build the sandbox
  launcher.
- `newuidmap` and `newgidmap` on `PATH` (the `shadow` / `uidmap` package). Bobr
  runs each builder in a Linux user namespace when you are not root, and uses
  these setuid helpers to set up the uid/gid map. As root — or under `podman
  unshare` — bobr uses its in-process host runtime and needs neither.
- `mkfs.erofs` (the `erofs-utils` package) — only if you build EROFS
  root-filesystem images (the `ErofsRootfs` builder).
- `nickel` — only for the recipe workflow below.

## Build bobr

```sh
git clone https://github.com/aleksey-makarov/bobr
cd bobr
cargo build                    # builds target/debug/bobr and target/debug/fsobj-hash
cargo build-sandbox-launcher   # builds the static musl sandbox launcher
```

`cargo build-sandbox-launcher` is a workspace alias that builds
`bobr-sandbox-launcher` for the musl target; `bobr` locates the launcher next to
its own binary, so no further setup is needed. The launcher is used only by the
`Sandbox` builder, so you can skip it until you build something that runs
commands.

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
./target/debug/bobr < hello.json
```

bobr prints the object's hash:

```text
354650604fa434e975eff93f27d72f688fc1c41d839ab3e97a6c89ddc6381fb4
```

The hash is derived from the content, not from the path or name, so you will get
the same value. The result is in the store:

```sh
$ cat /tmp/bobr-store/objects/354650604fa434e975eff93f27d72f688fc1c41d839ab3e97a6c89ddc6381fb4
hello, bobr
$ readlink /tmp/bobr-store/object-refs/hello
../objects/354650604fa434e975eff93f27d72f688fc1c41d839ab3e97a6c89ddc6381fb4
```

Because the single entry is one top-level file, the object is that file; a tree
with more entries would produce a filesystem-tree object instead (see
[Filesystem trees](./FS_TREE.md)). The `object-refs/hello` symlink is the
human-facing name for the result (see [Store](./STORE.md)).

## Building a real target

Writing requests by hand does not scale; real targets are authored in
[Nickel](https://nickel-lang.org/) in the separate **bobr-recipes** repository
and lowered to a request. Clone it next to `bobr` (the tooling expects the two
as siblings):

```sh
git clone https://github.com/aleksey-makarov/bobr-recipes
```

Then build one package attribute with the driver, which refreshes the local
hash locks, exports the request through `request.ncl`, and runs `bobr`:

```sh
cd bobr-recipes
tools/bobr-build.sh gzip
```

List the available attributes with `tools/list-pkgs-attrs.sh`. Useful options:

- `--store PATH` — where to build (default: `../bobr-store`, next to the repos);
- `--jobs N`, `--quiet` — the request's top-level knobs;
- `--podman-unshare` — run under `podman unshare` on hosts that forbid
  unprivileged user namespaces.

The first real build bootstraps a toolchain from source (glibc, gcc, …), so it
takes a while; later builds reuse cached objects and rebuild only what changed.
The result lands in the store under `object-refs/<name>`.

To author or extend recipes, see [Recipes in Nickel](./NICKEL.md).

## Rebuilding the world

`tools/bobr-rebuild-world.sh [attr]` builds into a fresh, timestamped store
(`bobr-store.<timestamp>`, with the `bobr-store` symlink repointed at it), seeds
source objects from the previous store by hardlink, records the `bobr` and
`bobr-recipes` commits, and runs the build through `bobr-build.sh`.

## Next steps

- [Concepts](./CONCEPTS.md) — content addressing, objects, keys, and recipes.
- [Request](./REQUEST.md) — the request format and the built-in builders.
- [Recipes in Nickel](./NICKEL.md) — authoring recipes.
- [Store](./STORE.md) — how results are stored, named, and reused.
