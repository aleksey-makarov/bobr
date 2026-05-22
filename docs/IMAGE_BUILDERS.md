# OCI Image Inputs

## Summary

`mbuild` keeps imported OCI images in the content-addressed store. The active
OCI and rootfs-backed execution path consists of:

- `Source` with `origin.type = "oci-registry"`: import one pinned external
  image from a registry into the store as an OCI image layout directory
- `OciExtract`: extract one OCI image layout input into an fs-tree object
- `Sandbox`: execute an explicit step plan against a readonly fs-tree rootfs
  input with `mbuild-runtime` and libcontainer

There is no active builder for producing derived OCI image layouts from fs-tree
inputs. Root filesystem composition is performed through fs-tree builders such
as `TreeMerge`, `ErofsRootfs`, and `Initramfs`.

The store, not the local container runtime image store, is the source of truth
for imported image contents. Step execution uses rootfs inputs, not OCI image
inputs.

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

The result can be consumed by `TreeMerge`, `ErofsRootfs`, `Initramfs`, or
`Sandbox` as a rootfs/tree input.

## `Sandbox`

`Sandbox` accepts:

- required `rootfs`: one fs-tree object used as the readonly root filesystem
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
- `mbuild` does not currently provide a builder for producing derived OCI
  image layouts from fs-tree inputs
- `Sandbox` requires a prepared fs-tree rootfs object
