# Image Builders

## Summary

`mbuild` currently implements store-owned OCI images for both imported images
and images built from filesystem tree inputs.

The current image path has these builders plus one `Source` origin:

- `Source` with `origin.type = "oci-registry"`: import one pinned external
  image from a registry into the store as an OCI image layout directory
- `Image`: build one derived OCI image layout directory from filesystem tree
  inputs, optionally on top of a base image
- `Binary`: execute an explicit step plan inside an OCI image layout input by
  loading the OCI layout into `podman` and then running ordered commands
  inside one long-lived container
- `Container`: execute the same explicit step plan contract against a rootfs
  directory input with `bwrap`
- `Sandbox`: execute the same explicit step plan contract against a rootfs
  directory input with `mbuild-runtime` and libcontainer

This means:

- the store, not the local `podman` image store, is the source of truth for
  imported and built image contents
- `podman` is still part of the current execution path for `Binary`
- `Container` does not consume an OCI image; it consumes a directory rootfs
  object and runs one `bwrap` process per configured step
- `Sandbox` does not consume an OCI image; it consumes a directory rootfs
  object and runs all steps in one libcontainer lifecycle
- file-composition conflict detection is not yet part of the `Image` builder

## `Source/oci-registry`

Imported registry images use a `Source` node like this:

```json
{
  "name": "host-image",
  "tag": "Source",
  "object_hash": "<oci-layout object hash>",
  "origin": {
    "type": "oci-registry",
    "image": "docker.io/library/buildpack-deps:bookworm",
    "digest": "sha256:<pinned manifest-or-index digest>"
  },
  "meta": {}
}
```

Current behavior:

- parses the image reference and talks directly to the OCI/Docker registry API
- handles Bearer auth challenges
- fetches the pinned manifest by digest
- if the pinned object is a manifest list, selects the `linux/amd64` manifest
- downloads the manifest, config blob, and all layer blobs
- verifies the digest of every downloaded blob
- writes the result to the staged object path as an OCI image layout directory
- writes `index.json` without image-ref annotations, so the canonical object is
  independent of the registry mirror named by `origin.image`
- stores no image-specific metadata in the canonical `Source` result record

The realized object is a directory with the standard OCI layout shape:

```text
<object>/
  oci-layout
  index.json
  blobs/sha256/...
```

`Source/oci-registry` currently targets `linux/amd64` only.

## `Image`

`Image` accepts:

- optional `base`: one OCI image layout directory
- one or more named filesystem tree inputs
- layer order follows lexical input name order after `base`

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

## Step Executors

`Binary`, `Container`, and `Sandbox` share the same step execution shape:

- `steps`: a non-empty ordered list of execution steps
- `script_config`: optional structured string payload serialized into
  `/__mbuild/config`

`Binary` and `Container` also accept legacy `install` metadata for their
published directory output. `Sandbox` does not accept `install`; final
ownership and modes are whatever the sandbox steps created under
`/__mbuild/out`.

Each step contains:

- `name`
- `run_as`
- `cwd`
- `argv`
- optional `env`

These builders perform controlled interpolation in `cwd`, `argv`, and
step-local environment values. Supported variables are:

- `@{config}`
- `@{build}`
- `@{out}`
- any named input placeholder such as `@{script}`, `@{source}`, `@{patch}`

Use `@@{name}` to emit the literal text `@{name}` without interpolation.
Legacy shell-style `${...}` remains plain text and is not interpreted by the
builder.

Interpolation is a simple path substitution:

- no shell parsing
- no word splitting
- no globbing
- no command substitution
- unknown variables are rejected as invalid builder config

The runtime does not assign package semantics to step names. Build-system
knowledge lives in script libraries and recipe helpers. Common recipe helpers
may still synthesize default step sequences such as:

- `configure`
- `build`
- `install`
- `post_install`

but these are authoring conventions, not special runtime phases.

Whether a build uses an out-of-tree build directory is a property of the
selected build script:

- `meson` uses `MBUILD_BUILD_DIR`
- `gnu-make` builds in-tree by default
- `autotools` builds out-of-tree by default and can opt out via script config

### `Binary` With An OCI Image Layout

`Binary` executes against an OCI image layout input in two stages:

1. read the OCI layout directory from the store
2. make that image available to `podman`, then create and start one container
   instance and execute the configured step list inside it with `podman exec`

Current behavior:

- validates that the `image` input resolves to an OCI layout directory
- reads `index.json` and the manifest blob
- extracts `config.digest`
- checks `podman image exists <config.digest>`
- if missing, creates a tar archive of the OCI layout and runs
  `podman load --input <tar>`
- runs `podman create`
- runs `podman start`
- runs the configured steps with `podman exec`
- removes the container with `podman rm --force`

The current execution path uses the OCI config digest as the runtime image
reference passed to `podman`.

`Binary` mounts:

- ordinary named inputs under `/__mbuild/inputs/<name>`
- the serialized `script_config` directory under `/__mbuild/config`

`/__mbuild/build` and `/__mbuild/out` are container-private writable directories created
inside the container lifecycle, not host bind mounts.

`Binary` also exports:

- `MBUILD_CONFIG_DIR=/__mbuild/config`
- `MBUILD_BUILD_DIR=/__mbuild/build`
- `MBUILD_OUT_DIR=/__mbuild/out`
- `MBUILD_STEP_NAME=<step name>`

`Binary.config` accepts an explicit linear execution plan:

```json
{
  "steps": [
    {
      "name": "configure",
      "run_as": "build-user",
      "cwd": "@{build}",
      "argv": ["@{script}", "configure"]
    },
    {
      "name": "build",
      "run_as": "build-user",
      "cwd": "@{build}",
      "argv": ["@{script}", "build"]
    },
    {
      "name": "install",
      "run_as": "root",
      "cwd": "@{build}",
      "argv": ["@{script}", "install"]
    }
  ],
  "script_config": { "...": "..." }
}
```

`Binary` interprets only execution mechanics:

- `run_as=build-user` executes as the container default user inside
  `--userns=keep-id`
- `run_as=root` executes as `--user 0:0`
- steps run strictly in order
- the build stops at the first failed step

This means:

- directory input mutations inside `/__mbuild/inputs/<name>` survive across all steps of
  one `Binary` build because all steps run in the same container lifecycle
- the realized result is exported from `/__mbuild/out` via `podman cp` after the
  final successful step
- live filesystem changes outside `/__mbuild/out` are not published automatically

### `Container` With A Rootfs Directory

`Container` executes the same step contract against a rootfs directory object.
It accepts:

- required `rootfs`: one directory object exposed as a writable overlay under
  `/`
- any number of extra named file or directory inputs mounted under
  `/__mbuild/inputs/<name>`

`Container` does not load or create an OCI image. It starts one `bwrap` process
per step, in strict step order. The host build temp directory contains:

- `build`
- `out`
- `config`
- `rootfs-overlay/upper`
- `rootfs-overlay/work`
- `input-overlays/<input-name>/upper`
- `input-overlays/<input-name>/work`

For every step, `Container` creates this sandbox shape:

- `--unshare-user`
- `--unshare-pid`
- `--unshare-net`
- `--overlay-src <rootfs>`
- `--overlay <rootfs_upper> <rootfs_work> /`
- `--proc /proc`
- `--dev /dev`
- `--tmpfs /tmp`
- `--dir /__mbuild`
- `--dir /__mbuild/inputs`
- `--ro-bind <config_dir> /__mbuild/config`
- `--bind <build_dir> /__mbuild/build`
- `--bind <out_dir> /__mbuild/out`

File inputs are read-only binds:

```text
--ro-bind <input> /__mbuild/inputs/<name>
```

Directory inputs use a per-build overlay:

```text
--overlay-src <input>
--overlay <upper> <work> /__mbuild/inputs/<name>
```

The rootfs source store object remains unchanged. The rootfs overlay `upper`
and `work` directories are reused across all steps of one `Container` build, so
mutations to `/` are visible to later steps in that same build. Rootfs overlay
state is temporary build state and is discarded with the build temp directory.

Directory input overlays follow the same lifetime rule. The input source store
object remains unchanged. The overlay `upper` and `work` directories are reused
across all steps of one `Container` build, so mutations to a directory input are
visible to later steps in that same build. The overlay state is temporary build
state and is discarded with the build temp directory.

`Container` exports the same core environment variables as `Binary`:

- `MBUILD_CONFIG_DIR=/__mbuild/config`
- `MBUILD_BUILD_DIR=/__mbuild/build`
- `MBUILD_OUT_DIR=/__mbuild/out`
- `MBUILD_STEP_NAME=<step name>`

It runs with `--clearenv`, then sets a fixed baseline `PATH`, `HOME`, and
`USER`, then applies step-local `env` entries after interpolation. Step-local
entries override baseline values.

`run_as=build-user` runs the bwrap process with the current host uid and gid.
`run_as=root` runs it as uid `0`, gid `0` inside the user namespace.

The staged result is the host temp `out` directory. It must exist and be a
directory after all steps complete. Install metadata defaults to the same
`**` rule used by `Binary` when `install` is omitted.

### `Sandbox` With A Rootfs Directory

`Sandbox` executes the same step contract against a rootfs directory object.
It accepts:

- required `rootfs`: one directory object exposed read-only under `/`
- any number of extra named file or directory inputs mounted under
  `/__mbuild/inputs/<name>`

`Sandbox` uses `mbuild-runtime` and libcontainer directly. It creates one
container lifecycle for the whole build, then runs each configured step as a
separate tenant exec operation inside that lifecycle.

Top-level rootfs directories are mounted as read-only recursive bind mounts,
and top-level rootfs files are mounted as read-only bind mounts. Top-level
rootfs symlinks are materialized in the temporary bundle rootfs before the
sandbox starts. The source rootfs store object is never mutated, and `Sandbox`
does not create rootfs overlay state or perform runtime copy-up for rootfs
paths.

`/__mbuild/build` is a writable host temporary bind mount owned inside the
sandbox by the numeric build user `1:1`. `/__mbuild/out` is a writable host
bind mount and is the only published output path.

File and directory inputs are read-only bind mounts. Directory inputs are
mounted recursively and remain immutable for the whole sandbox lifecycle. The
store object is never mutated, and `Sandbox` does not create per-input overlay
state or perform runtime copy-up for inputs.

Any step that needs a mutable source tree must copy or unpack it into
`/__mbuild/build` first. Before user steps run, `Sandbox` creates only
`/__mbuild/build` as a writable directory owned by `1:1`; it does not change
ownership under `/__mbuild/inputs/<name>`.

The sandbox mount policy is intentionally small:

- no host network connectivity
- `/proc` is mounted
- `/sys` has no special runtime mount
- `/tmp` and `/run` are tmpfs mounts
- hostname is `mbuild`

`Sandbox` exports the same core environment variables as the other step
executors, plus `TMPDIR=/tmp`.

`run_as=build-user` runs as numeric uid `1`, gid `1` with an empty capability
set. `run_as=root` runs as numeric uid `0`, gid `0` with only `CHOWN`,
`DAC_OVERRIDE`, `DAC_READ_SEARCH`, `FOWNER`, and `FSETID`.

After all steps complete, `Sandbox` scans `/__mbuild/out` inside the sandbox
with `lstat` and `readlink`, then publishes a canonical fs-tree object:

```text
manifest.jsonl
root/
```

The manifest records the actual sandbox namespace uid, gid, unix mode, entry
kind, and symlink target for every output path. The raw `/__mbuild/out`
directory becomes the object's `root/`; `Sandbox` does not normalize ownership
or modes on the host after runtime success.

`Sandbox` computes the fs-tree object hash inside the sandbox from the
canonical manifest bytes plus the existing `/__mbuild/out` tree and returns it
to the CAS as a precomputed hash. The realized result metadata is `{}`.

## Current Limitations

The current image path intentionally does not yet implement the final image
design.

In particular:

- `Binary` still depends on `podman load` and `podman`
- `Container` depends on `bwrap` support for `--overlay-src` and `--overlay`
- `Container` does not provide a `fuse-overlayfs` fallback
- `Sandbox` depends on libcontainer rootless runtime support
- `Image` does not compute or persist canonical flattened `contents`
- `Image` does not implement additive-only file-composition checks
- `Image` does not reject path conflicts between incoming filesystem tree
  inputs beyond the limited normalization already performed while building a
  tar layer

This document describes the current builder contract, not the planned future
execution model.
