# Bobr

<img src="https://bobr.build/bobr.svg" alt="bobr" width="200">

> `Bobr` is a content-addressed build system for Linux. It executes a request —
> a DAG of recipe nodes — and produces objects such as filesystem trees and
> root-filesystem images.

The accompanying recipe collection builds Linux kernels, initramfs images, and
complete root filesystems from declarative Nickel recipes. It also provides
helpers for booting the resulting systems under QEMU.

Website and documentation: [bobr.build](https://bobr.build/).

## Try Bobr

On x86-64 Linux, install the latest release with:

```sh
curl -fsSL https://bobr.build/install.sh | bash
```

Then follow [Build and boot a QEMU
system](https://bobr.build/GETTING_STARTED.html#build-and-boot-a-qemu-system).
With the public repository of prebuilt objects, the initial download normally
takes about 1–2 minutes, depending on the connection.

The installer verifies the release checksum and installs into
`~/.local/bin`. See [Installation](https://bobr.build/INSTALLATION.html) for a
review-before-running variant, custom destinations, prerequisites, and
uninstallation.

## Current status

Bobr is under active development and currently targets native x86-64 Linux.
Development has so far focused on the build system itself; the included guest
systems remain proof-of-concept demos.

## Key properties

Every object — a fetched source or a build result — is named by the hash of
its content and kept in a content-addressed store. Identical results are
deduplicated and reused across builds, so a request rebuilds only what is
actually missing and serves everything else from the store. Reproducible builds
with the same inputs produce the same content-addressed object. Filesystem trees
are assembled by hard-linking files into place, not copying — so they build fast
and cheap.

Builders run in Linux user-namespace sandboxes with an explicit root filesystem
and a controlled environment, so a build cannot silently depend on the host.
Sources are pinned by content hash (HTTP fetches support mirror fallback). A
handful of composable builders cover the common needs — assembling and
transforming filesystem trees, and extracting OCI images — and a sandbox builder
runs arbitrary build steps, from compiling a package to packing a tree into a
bootable image. Recipes are written declaratively (in [Nickel](https://nickel-lang.org/))
and lower to the JSON request `bobr` executes.

## Related systems

Bobr addresses some of the same system-construction problems as
[Yocto](https://www.yoctoproject.org/), but keeps recipes in a separate Nickel
layer with contracts and Nix-style overlays. Yocto is centered on
cross-compilation for embedded targets; Bobr currently builds ordinary systems
natively.

Like [Nix](https://nixos.org/), Bobr provides hermetic builds, hash-keyed
storage, and extensive reuse. Unlike Nix, Bobr does not prescribe a Nix-style
filesystem layout: filesystem structure is selected by recipes. This makes
conventional layouts possible, at the cost of not keeping several package
versions side by side in the resulting system. Bobr is content-addressed from
the start, whereas Nix store paths are input-addressed and content-addressed
derivations remain experimental.

Bobr borrows ideas from [OSTree](https://ostreedev.github.io/ostree/) — a
content-addressed store and an immutable read-only rootfs. But OSTree is about
delivering and updating that OS on the machine (atomic upgrades and rollback),
where Bobr builds the images.

Bobr [HostBundles](https://bobr.build/HOST_BUNDLE.html) are a proof of concept
for container-like portability without a container runtime. Their practical
viability still needs to be evaluated.

## Independence and Affiliation

This is a personal open-source project, developed independently rather than
derived from an existing codebase. It is not affiliated with or endorsed by
Qualcomm or any other organization.

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](https://github.com/aleksey-makarov/bobr/blob/master/LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](https://github.com/aleksey-makarov/bobr/blob/master/LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
