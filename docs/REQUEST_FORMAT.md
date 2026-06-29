# Request and store format

`bobr` executes one JSON DAG request.

The input document is a JSON envelope with:

- `options`
- `nodes`

`nodes` is a table of recipe nodes keyed by technical ids. The root build
target is the entry with the reserved id `root`. Dependencies are encoded as
id references in input slots. `options.store` points at the store root that
`bobr` should use for this request. `bobr` parses that DAG request,
validates each node, performs top-down store lookups, and materializes only the
missing nodes. Missing leaves and other ready nodes may execute in parallel.

There are two node classes.

Builder nodes use the generic builder shape:

```json
{
  "options": {
    "store": "/abs/path/to/store",
    "quiet": false,
    "jobs": 8
  },
  "nodes": {
    "root": {
      "name": "tar-1.35",
      "tag": "Sandbox",
      "config": {
        "steps": [
          {
            "name": "build",
            "run_as": "build-user",
            "cwd": "@{build}",
            "argv": ["@{script}", "build"]
          }
        ]
      },
      "inputs": {
        "rootfs": "rootfs_1",
        "script": "script_1",
        "source": "src_0"
      }
    }
  }
}
```

Builder node payload fields:

- `name`: publication name
- `tag`: builder tag from the Rust builder registry
- `config`: opaque builder payload
- `inputs`: object keyed by named input dependencies

Nickel recipes may use higher-level synthetic tags before lowering. The
package-aware helpers `Autotools`, `Makefile`, `Meson`, `PerlModule`, and
`SandboxBuild` inject a generated build rootfs and lower to Rust-side `Sandbox`
requests. The explicit-rootfs helper tags are `AutotoolsRootfs`,
`MakefileRootfs`, `MesonRootfs`, `PerlModuleRootfs`, and
`SandboxBuildRootfs`.

Input encoding is generic:

- every present input value is one node id string
- optional inputs are represented by field absence
- ordered extra inputs are expressed by sortable names such as `in000`,
  `in001`, ...

The runtime rejects:

- unknown builder tags
- missing required inputs
- extra inputs for builders that do not allow them
- non-string input values

`Group` is the phony aggregate builder for requests that need one root but
must realize several otherwise unrelated targets. It has empty config and one
or more arbitrary inputs:

```json
{
  "name": "all-targets",
  "tag": "Group",
  "config": {},
  "inputs": {
    "in000": "toolchain",
    "in001": "rootfs",
    "in002": "image"
  }
}
```

`Group` does not merge or inspect input payloads. It stages a constant
zero-byte marker file after all inputs have been realized, so the root
`RealizedObject` is only a completion marker. The meaningful artifacts are the
input targets and their normal `object-refs/`.

`Source` is a separate execution class with its own shape:

```json
{
  "options": {
    "store": "/abs/path/to/store",
    "quiet": false,
    "jobs": 8
  },
  "nodes": {
    "root": {
      "name": "linux-src",
      "tag": "Source",
      "object_hash": "0123...abcd",
      "origin": {
        "tag": "Http",
        "url": [
          "https://example.invalid/linux.tar.xz"
        ],
        "unpack": true
      }
    },
  }
}
```

`Source` has:

- no `config`
- no `inputs`
- a `build_key` equal to its declared `object_hash`

In v1, `Source` supports three origins:

- `origin.tag = "Path"`
- `origin.path` must be an absolute host path
- `origin.unpack` defaults to `false`; when true, the local path is treated as a tar archive
- `origin.tag = "Http"`
- `origin.url` is one HTTP(S) URL or an ordered fallback list
- `origin.unpack` defaults to `false`
- `origin.archive_format` may override archive detection for unpacked sources
- `origin.tag = "OciRegistry"`
- `origin.image` is the registry image locator kept in the recipe
- `origin.digest` is the pinned manifest or index digest requested from the registry
- `origin.platform` selects the OCI platform when the pinned digest names a
  manifest list or OCI index

`Source` may also omit `origin`. In that shape, the payload object must
already exist in the store under `objects/<object_hash>`. If the canonical
`<store>/object-records/<object_hash>.json` record is missing, `bobr` reconstructs
it from the declared object hash. A successful source lookup also creates or
repairs `builds/<object_hash>`.

If a source origin materializes a different object than the declared
`object_hash`, `bobr` still imports the actual object into
`objects/<actual_hash>`, but it does not write the canonical object record or
source build handle for the declared hash. The failing message includes the
actual hash so the recipe can be updated and rerun without downloading again.

CLI contract:

- `bobr [recipe.json]`
- if `recipe.json` is omitted, the JSON envelope is read from `stdin`
- on success, `stdout` receives the realized root `RealizedObject` as JSON
- live progress goes to `stderr` unless `options.quiet` is true
- `options.jobs` limits parallel builder execution; if omitted, the default is
  the available CPU parallelism
- `options.store` sets the store root for the request
- command-line arguments do not override JSON options; to change build options,
  change the input JSON envelope

The final store path must be an absolute path to an existing directory. That
directory is the store root itself. A request may still choose a path named
`.bobr`, but `bobr` no longer adds an extra `.bobr/` layer implicitly.

The store layout is content-addressed:

- `<store>/objects/` stores payload objects by `object_hash`
- `<store>/fs-files/` stores regular-file payloads referenced by fs-tree
  manifest objects
- `<store>/fs-trees/` caches materialized fs-tree roots by manifest
  `object_hash`
- `<store>/fs-tree-refs/` stores user-facing refs to named materialized
  fs-tree roots
- `<store>/object-records/` stores canonical object records by `object_hash`
- `<store>/reuses/` stores builder-only canonical reuse refs by `reuse_key`
- `<store>/builds/` stores public build handles by `build_key`
- `<store>/object-refs/` stores user-facing refs to named successful objects

`<store>/object-refs/<name>` always points at
`../objects/<object_hash>`, regardless of object kind. Filesystem tree builder
results store the canonical fs-tree manifest text as the object payload.
Referenced regular-file payloads live under `<store>/fs-files/`, and
materialized roots under `<store>/fs-trees/` are cache entries rather than
object-ref targets.
Named fs-tree materializations also update `<store>/fs-tree-refs/<name>` for
inspection. Runtime cache lookup does not read these refs.

`build_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `build_key`s

For `Source`, `build_key` is the declared `object_hash` reinterpreted as a
build key.

`reuse_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `object_hash` values

The dependency order comes from the builder input contract:

- reserved inputs in spec order
- extra inputs in lexical name order

It does not depend on JSON field order or node id order. This lets `bobr`
keep the general runtime independent from concrete builders.

Concrete builder behavior is documented separately:

- OCI image inputs: [`docs/IMAGE_BUILDERS.md`](./IMAGE_BUILDERS.md)
- filesystem-related builders: [`docs/ROOTFS_BUILDERS.md`](./ROOTFS_BUILDERS.md)

For the architecture documents, start with [`docs/OVERVIEW.md`](./OVERVIEW.md).
