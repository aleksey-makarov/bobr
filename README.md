# Bobr

<img src="docs/bobr.svg" alt="bobr" width="200">

> `bobr` is a build system. It executes a request — a DAG of recipe nodes — and
> yields reproducible, content-addressed objects such as filesystem trees and
> root-filesystem images.

## Key properties

Every object — a fetched source or a build result — is named by the hash of
its content and kept in a content-addressed store. Identical results are
deduplicated and reused across builds, so a request rebuilds only what is
actually missing and serves everything else from the store. The same inputs
always produce the same object hash. Filesystem trees are assembled by
hard-linking files into place, not copying — so they build fast and cheap.

Builders run in Linux user-namespace sandboxes with an explicit root filesystem
and a controlled environment, so a build cannot silently depend on the host.
Sources are pinned by content hash (HTTP fetches support mirror fallback). A
handful of composable builders cover the common needs — assembling and
transforming filesystem trees, and extracting OCI images — and a sandbox builder
runs arbitrary build steps, from compiling a package to packing a tree into a
bootable image. Recipes are written declaratively (in [Nickel](https://nickel-lang.org/))
and lower to the JSON request `bobr` executes.

## What `bobr` is like

`bobr` is like Yocto, but with sane recipe syntax. Recipes are a separate
Nickel layer — a real type-and-contract system, with Nix-style overlays for
composing them. And where Yocto is built around cross-compiling for embedded
targets, `bobr` so far builds ordinary systems natively.

`bobr` is like Nix, but with sane filesystem structure — a recipe choice, not a
`bobr` constraint; the same engine could produce a Nix-style store. The trade-off
is that such a layout can't keep several versions of a package side by side, the
way Nix can; in exchange it should feel more like a normal system and be more
convenient to use. Like Nix, `bobr` keeps results in a hash-keyed store with
hermetic, sandboxed builds and heavy deduplication. Unlike Nix, it is
content-addressed from the start, whereas Nix store paths are input-addressed
and content-addressed derivations remain experimental.

`bobr` borrows ideas from OSTree — a content-addressed store, and an immutable
read-only rootfs. But OSTree is about delivering and updating that OS on the
machine (atomic upgrades, rollback), where `bobr` builds the images.

`bobr` HostBundles offer container-like portability without a container
runtime: a relocatable directory carrying the program, its loader, libraries,
data, and a tiny static launcher. Run it from the store or copy it to another
compatible Linux machine — it keeps using its bundled runtime, not the host's.

## Documentation

Documentation lives in [`docs/`](./docs/). New here? Start with
[Getting Started](./docs/GETTING_STARTED.md); for the full table of
contents see [`docs/CONTENTS.md`](./docs/CONTENTS.md).

## Independence and Affiliation

This project is an independent personal open-source effort.
It is not affiliated with, derived from, or endorsed by Qualcomm or the Yocto Project.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](./LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
