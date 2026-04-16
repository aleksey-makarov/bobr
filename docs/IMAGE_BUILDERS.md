# Image Builders

## Summary

`mbuild` currently implements store-owned OCI images for both imported images
and images built from filesystem tree inputs.

The current image path has three builders:

- `ContainerImage`: import one pinned external image from a registry into the
  store as an OCI image layout directory
- `Image`: build one derived OCI image layout directory from filesystem tree
  inputs, optionally on top of a base image
- `Binary`: execute a build script inside an OCI image layout input by loading
  the OCI layout into `podman` and then running phased commands inside one
  long-lived container

This means:

- the store, not the local `podman` image store, is the source of truth for
  imported and built image contents
- `podman` is still part of the current execution path for `Binary`
- file-composition conflict detection is not yet part of the `Image` builder

## `ContainerImage`

`ContainerImage` accepts this config:

```json
{
  "image": "docker.io/library/buildpack-deps:bookworm",
  "digest": "sha256:<pinned digest>"
}
```

It does not accept inputs.

Current behavior:

- parses the image reference and talks directly to the OCI/Docker registry API
- handles Bearer auth challenges
- fetches the pinned manifest by digest
- if the pinned object is a manifest list, selects the `linux/amd64` manifest
- downloads the manifest, config blob, and all layer blobs
- verifies the digest of every downloaded blob
- writes the result to the staged object path as an OCI image layout directory

The realized object is a directory with the standard OCI layout shape:

```text
<object>/
  oci-layout
  index.json
  blobs/sha256/...
```

The current realized result metadata contains:

- `manifest_digest`: the digest of the manifest blob stored in the realized
  OCI layout

`ContainerImage` currently targets `linux/amd64` only.

## `Image`

`Image` accepts:

- optional `base`: one OCI image layout directory
- repeated `inputs`: one or more filesystem tree directories

Config fields:

- `mode`: optional, `bootstrap` or `layered`
- `ref_name`: optional image name annotation for the OCI index

Mode selection:

- no base → `bootstrap`
- base present → `layered`
- explicit `mode` must match the presence or absence of `base`

### Bootstrap Mode

Bootstrap mode creates a new OCI image layout from scratch:

- collects all files, directories, and symlinks from the incoming
  input directories
- builds one deterministic tar layer from those paths
- compresses it with gzip
- computes one `diff_id` from the uncompressed tar
- writes one OCI config blob with one rootfs layer
- writes one OCI manifest and `index.json`

### Layered Mode

Layered mode builds a new OCI image layout on top of a base image:

- reads the base manifest and base config from the base OCI layout directory
- creates a new OCI layout directory
- hardlinks the base layer blobs into the new layout
- creates one new deterministic layer from the incoming input directories
- writes one new OCI config blob by extending `rootfs.diff_ids`
- writes one new OCI manifest by appending the new layer to the base layers
- writes a new `index.json`

The current realized result metadata contains:

- `manifest_digest`: the digest of the newly written OCI manifest blob

## `Binary` With An OCI Image Layout

`Binary` executes against an OCI image layout input in two stages:

1. read the OCI layout directory from the store
2. make that image available to `podman`, then create and start one container
   instance and execute build phases inside it with `podman exec`

Current behavior:

- validates that the `image` input resolves to an OCI layout directory
- reads `index.json` and the manifest blob
- extracts `config.digest`
- checks `podman image exists <config.digest>`
- if missing, creates a tar archive of the OCI layout and runs
  `podman load --input <tar>`
- runs `podman create`
- runs `podman start`
- runs the build phases with `podman exec`
- removes the container with `podman rm --force`

The current execution path uses the OCI config digest as the runtime image
reference passed to `podman`.

`Binary` mounts:

- source inputs under `/in/sources*`
- the build script
- the serialized `script_config` directory

`/work/build` and `/out/out` are container-private writable directories created
inside the container lifecycle, not host bind mounts.

The canonical source directory is:

- `MBUILD_SOURCE_DIR=/in/sources0`

`Binary` also exports:

- `MBUILD_BUILD_DIR=/work/build`
- `MBUILD_INSTALL_DIR=/out/out`
- `MBUILD_PHASE=<configure|build|install|post_install>`

The current phased execution contract is:

- `configure`: executed as the container default user inside
  `--userns=keep-id`
- `build`: executed as the container default user inside `--userns=keep-id`
- `install`: executed as `--user 0:0`
- `post_install`: executed as `--user 0:0`

This means:

- source-tree mutations inside `/in/sources0` survive across all phases of one
  `Binary` build because all phases run in the same container lifecycle
- the realized result is exported from `/out/out` via `podman cp` after a
  successful `post_install`
- live filesystem changes outside `/out/out` are not published automatically

Whether a build uses an out-of-tree build directory is a property of the
selected build script:

- `meson` uses `MBUILD_BUILD_DIR`
- `gnu-make` builds in-tree by default
- `autotools` builds out-of-tree by default and can opt out via script config

## Current Limitations

The current image path intentionally does not yet implement the final image
design.

In particular:

- `Binary` still depends on `podman load` and `podman`
- `Image` does not compute or persist canonical flattened `contents`
- `Image` does not implement additive-only file-composition checks
- `Image` does not reject path conflicts between incoming filesystem tree
  inputs beyond the limited normalization already performed while building a
  tar layer

This document describes the current builder contract, not the planned future
execution model.
