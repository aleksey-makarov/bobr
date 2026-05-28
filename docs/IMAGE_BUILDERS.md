# OCI Image Inputs

## Summary

`mbuild` keeps imported OCI images in the content-addressed store. The active
OCI and rootfs-backed execution path consists of:

- `Source` with `origin.tag = "OciRegistry"`: import one pinned external
  image from a registry into the store as an OCI image layout directory
- `OciExtract`: extract one OCI image layout input into an fs-tree object
- `Sandbox`: execute an explicit step plan against a readonly fs-tree rootfs
  input with the `mbuild-runtime` sandbox launcher

There is no active builder for producing derived OCI image layouts from fs-tree
inputs. Root filesystem composition is performed through fs-tree builders such
as `TreeMerge`, `ErofsRootfs`, and `Initramfs`.

The store, not the local container runtime image store, is the source of truth
for imported image contents. Step execution uses rootfs inputs, not OCI image
inputs.

## `Source/OciRegistry`

Imported registry images use a `Source` node like this:

```json
{
  "name": "host-image",
  "tag": "Source",
  "object_hash": "<oci-layout object hash>",
  "origin": {
    "tag": "OciRegistry",
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

`Source/OciRegistry` currently targets `linux/amd64` only.

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

The `rootfs` input must be a valid fs-tree object. Sandbox execution consumes
that object's `root/` directory as the container root filesystem after
validating it against `manifest.jsonl`.

Extra inputs are ordinary store objects from the sandbox point of view. They
are mounted as the whole realized object path. If an extra input is a directory
that happens to contain `manifest.jsonl` and `root/`, `Sandbox` does not treat
it as an fs-tree payload and does not mount `root/` instead of the object
directory.

Extra input names are also interpolation variable names. They must start with
an ASCII letter or `_`, and the remaining characters must be ASCII letters,
digits, or `_`. The names `build`, `out`, and `config` are reserved.

### `Sandbox.config`

`Sandbox.config` is a JSON object with these fields:

- `steps`: required non-empty array of step objects
- `script_config`: optional config tree materialized at `/__mbuild/config`

Unknown fields are rejected. In particular, `Sandbox.config` does not accept
`install` metadata; filesystem ownership and modes are represented by the
output manifest produced by the runtime.

Each step object has this shape:

- `name`: non-empty string after trimming; used in reports and log names
- `run_as`: `"build-user"` or `"root"`
- `cwd`: non-empty string before interpolation; must resolve to an absolute path
- `argv`: non-empty array of non-empty strings
- `env`: optional object whose values must be strings

`cwd`, every `argv` item, and every `env` value support interpolation. `name`,
`run_as`, `env` keys, and `script_config` do not support interpolation.

Supported interpolation variables:

- `@{build}`: writable build directory
- `@{out}`: writable output directory
- `@{config}`: materialized `script_config` directory
- `@{<input>}`: readonly mount for an extra named input

`@@{name}` escapes interpolation and renders the literal text `@{name}`.
Unknown variables, malformed interpolation expressions, and interpolation
names that are not valid input names are invalid config.

`script_config` may be absent or `null`, which creates an empty config
directory. Otherwise it is a recursive tree:

- JSON objects become directories
- JSON arrays become directories with zero-padded numeric entries such as
  `00000000`, preserving array order lexically
- JSON strings become file contents

Object keys in `script_config` must be non-empty, must not be `.` or `..`, and
may contain only ASCII letters, digits, `.`, `_`, and `-`.

Example:

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

The published `Sandbox` result is an fs-tree object.

Package-aware synthetic recipe helpers lower to `Sandbox`:

- `Autotools`
- `Makefile`
- `Meson`
- `PerlModule`
- `SandboxBuild`

Package-aware helpers require `deps = { build = [...], runtime = [...] }`.
They do not require or consume `inputs.rootfs`; the Nickel lowering layer
builds a temporary `TreeMerge` rootfs from `base_filesystem`, the runtime
closure of the helper's default build tools, and the runtime closure of
`deps.build`, then injects that rootfs into the lowered runtime `Sandbox`
request. The published package runtime dependencies remain the recipe's
`deps.runtime`.

Explicit-rootfs synthetic recipe helpers are also available:

- `AutotoolsRootfs`
- `MakefileRootfs`
- `MesonRootfs`
- `PerlModuleRootfs`
- `SandboxBuildRootfs`

These helpers require `inputs.rootfs` and use it as supplied. They remain
available for bootstrap recipes and other cases where the caller must choose
the execution rootfs directly.

Default build tools:

- `Autotools`: the common native toolchain plus `autoconf`, `m4`, and `perl`
- `Makefile`: the common native toolchain
- `Meson`: the common native toolchain plus `pkgconf` and `python`
- `PerlModule`: the common native toolchain plus `perl`
- `SandboxBuild`: `bash`, `tar`, `gzip`, `bzip2`, `xz`, and `patch`

The common native toolchain is `linux_headers`, `glibc`, `binutils`, `gcc`,
`bash`, `make`, `coreutils`, `gawk`, `sed`, `grep`, `tar`, `gzip`, `xz`,
`bzip2`, `patch`, `findutils`, and `diffutils`.

## Current Limitations

- `Source/OciRegistry` currently selects only `linux/amd64`
- `mbuild` does not currently provide a builder for producing derived OCI
  image layouts from fs-tree inputs
- Rust-side `Sandbox` requests require a prepared fs-tree rootfs object and use
  its validated `root/` directory as the execution root
