# Image Builders

## Summary

`mbuild` keeps imported and built OCI images in the content-addressed store.
The active image and rootfs-backed execution path consists of:

- `Source` with `origin.type = "oci-registry"`: import one pinned external
  image from a registry into the store as an OCI image layout directory
- `Image`: build one derived OCI image layout directory from filesystem tree
  inputs, optionally on top of a base image
- `OciExtract`: extract one OCI image layout input into an fs-tree object
- `Sandbox`: execute an explicit step plan against a readonly rootfs directory
  input with `mbuild-runtime` and libcontainer

The store, not the local container runtime image store, is the source of truth
for imported and built image contents. Step execution uses rootfs inputs, not
OCI image inputs.

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
  }
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

`Source/oci-registry` currently targets `linux/amd64` only.

## `Image`

`Image` accepts:

- optional `base`: one OCI image layout directory
- one or more named filesystem tree inputs

`Image.config` accepts:

```json
{
  "mode": "bootstrap",
  "ref_name": "optional/name:tag"
}
```

Current behavior:

- `mode = "bootstrap"` creates a new image from the input filesystem trees
- `mode = "layered"` requires `base` and appends one layer with input tree
  contents
- when `mode` is omitted, the builder chooses `layered` if `base` is present
  and `bootstrap` otherwise
- extra inputs are consumed in lexical input name order
- the realized payload is an OCI layout directory
- the image manifest digest is recorded inside the OCI layout `index.json`

## `OciExtract`

`OciExtract` accepts one `image` input that resolves to an OCI layout
directory. It extracts the image root filesystem into an fs-tree object:

```text
manifest.jsonl
root/
oci-config.json
```

`manifest.jsonl` carries required `h` fields for file and symlink entries.
`oci-config.json` is top-level metadata: rootfs composition ignores it, but it
participates in the published object hash.

The result can be consumed by `TreeMerge`, `ErofsRootfs`, or `Sandbox` as a
rootfs/tree input.

## `Sandbox`

`Sandbox` accepts:

- required `rootfs`: one fs-tree or directory object used as the readonly root
  filesystem
- extra named inputs mounted read-only under `/__mbuild/inputs/<name>`

`Sandbox.config` accepts an explicit ordered step plan:

```json
{
  "script_config": {
    "configure_args": ["--disable-nls"]
  },
  "steps": [
    {
      "name": "build",
      "run_as": "build-user",
      "cwd": "@{build}",
      "argv": ["@{script}", "build"],
      "env": {
        "CC": "gcc"
      }
    }
  ]
}
```

Supported interpolation variables:

- `@{build}`: writable build directory
- `@{out}`: writable output directory
- `@{config}`: materialized `script_config` directory
- `@{<input>}`: readonly mount for an extra named input

The published `Sandbox` result is an fs-tree object. `Sandbox` does not accept
`install` metadata in config; filesystem ownership and modes are represented by
the output manifest produced by the runtime.

Synthetic recipe helpers lower to `Sandbox`:

- `Autotools`
- `Makefile`
- `Meson`
- `PerlModule`

These explicit-rootfs helpers require `inputs.rootfs` and use it as supplied.
They remain available for bootstrap recipes and other cases where the caller
must choose the execution rootfs directly.

Package-aware synthetic helpers are also available:

- `AutotoolsPackage`
- `MakefilePackage`
- `MesonPackage`
- `PerlModulePackage`
- `SandboxPackage`

Package helpers require `deps = { build = [...], runtime = [...] }`. They do
not require or consume `inputs.rootfs`; the Nickel lowering layer builds a
temporary `TreeMerge` rootfs from `base_filesystem`, the runtime closure of the
helper's default build tools, and the runtime closure of `deps.build`, then
injects that rootfs into the corresponding explicit-rootfs helper. The
published package runtime dependencies remain the recipe's `deps.runtime`.

Default build tools:

- `AutotoolsPackage`: the common native toolchain plus `autoconf`, `m4`, and
  `perl`
- `MakefilePackage`: the common native toolchain
- `MesonPackage`: the common native toolchain plus `pkgconf` and `python`
- `PerlModulePackage`: the common native toolchain plus `perl`
- `SandboxPackage`: `bash`, `tar`, `gzip`, `bzip2`, `xz`, and `patch`

The common native toolchain is `linux_headers`, `glibc`, `binutils`, `gcc`,
`bash`, `make`, `coreutils`, `gawk`, `sed`, `grep`, `tar`, `gzip`, `xz`,
`bzip2`, `patch`, `findutils`, and `diffutils`.

## Current Limitations

- `Source/oci-registry` currently selects only `linux/amd64`
- `Image` does not yet perform the same manifest-level conflict validation as
  `TreeMerge`
- `Sandbox` requires a prepared rootfs directory or fs-tree object
