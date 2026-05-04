# mbuild

`mbuild` executes one JSON DAG request.

The input document is a JSON envelope with:

- `paths`
- `options` (optional)
- `nodes`

`nodes` is a table of recipe nodes keyed by technical ids. The root build
target is the entry with the reserved id `root`. Dependencies are encoded as
id references in input slots. `paths.store` points at the store root that
`mbuild` should use for this request. `mbuild` parses that DAG request,
validates each node, performs top-down store lookups, and materializes only
the missing nodes. Missing leaves and other ready nodes may execute in
parallel.

There are two node classes.

Builder nodes use the generic builder shape:

```json
{
  "paths": {
    "store": "/abs/path/to/store",
    "local": "/abs/path/to/local-sources"
  },
  "options": {
    "quiet": false,
    "jobs": 8
  },
  "nodes": {
    "root": {
      "name": "tar-1.35",
      "tag": "Binary",
      "config": {},
      "inputs": {
        "image": "image_1",
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

`Source` is a separate execution class with its own shape:

```json
{
  "paths": {
    "store": "/abs/path/to/store"
  },
  "options": {
    "quiet": false,
    "jobs": 8
  },
  "nodes": {
    "root": {
      "name": "linux-src",
      "tag": "Source",
      "object_hash": "0123...abcd",
      "origin": {
        "type": "http",
        "url": [
          "https://example.invalid/linux.tar.xz"
        ],
        "unpack": true
      },
      "meta": {}
    },
  }
}
```

`Source` has:

- no `config`
- no `inputs`
- no `build_key`

In v1, `Source` supports three origins:

- `origin.type = "path"`
- `origin.mode = "direct" | "tar"`
- `origin.path` must be a non-empty relative path without `..`
- `origin.type = "http"`
- `origin.url` is one HTTP(S) URL or an ordered fallback list
- `origin.unpack` defaults to `true`
- `origin.archive_format` may override archive detection for unpacked sources
- `origin.type = "oci-registry"`
- `origin.image` is the registry image locator kept in the recipe
- `origin.digest` is the pinned manifest or index digest requested from the registry
- pinned manifest lists / OCI indexes are resolved to the `linux/amd64` manifest

`Source.meta.install` carries install metadata for unpacked directory objects.

`Source` may also omit `origin`. In that shape, the payload object must
already exist in the store under `objects/<object_hash>`. If the canonical
`<store>/results/<result_id>.json` record is missing, `mbuild` reconstructs
it from the recipe metadata.

If a source origin materializes a different object than the declared
`object_hash`, `mbuild` still imports the actual object into
`objects/<actual_hash>`, but it does not write the canonical result record or
publish refs. The failing message includes the actual hash so the recipe can
be updated and rerun without downloading again.

CLI contract:

- `mbuild [recipe.json]`
- if `recipe.json` is omitted, the JSON envelope is read from `stdin`
- on success, `stdout` receives the realized root `RealizedResult` as JSON
- live progress goes to `stderr` unless `--quiet` is set
- `--jobs/-j` limits parallel builder execution; the default is the available
  CPU parallelism
- recipe-level `options.quiet` and `options.jobs` provide per-request defaults
  that are overridden by explicit CLI flags

`paths.store` must be an absolute path to an existing directory. That
directory is the store root itself. A request may still choose a path named
`.mbuild`, but `mbuild` no longer adds an extra `.mbuild/` layer implicitly.
`paths.local` is optional and is required only when the request contains a
`Source` with `origin.type = "path"`.

The store layout is content-addressed:

- `<store>/objects/` stores payload objects by `object_hash`
- `<store>/results/` stores canonical realized results by `result_id`
- `<store>/reuses/` stores builder-only canonical reuse refs by `reuse_key`
- `<store>/builds/` stores builder-only public build handles by `build_key`
- `<store>/meta-refs/` and `<store>/object-refs/` store published current refs

For filesystem tree objects, `<store>/object-refs/<name>` points at the
user-facing payload directory under `../objects/<object_hash>/root`. The
canonical identity path remains `../objects/<object_hash>`.

`build_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `build_key`s

`reuse_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency input identities

Each direct input identity contains:

- `object_hash`
- `meta_hash`

`result_id` is computed from:

- `object_hash`
- `meta_hash`

The dependency order comes from the builder input contract:

- reserved inputs in spec order
- extra inputs in lexical name order

It does not depend on JSON field order or node id order. This lets `mbuild`
keep the general runtime independent from concrete builders.

Concrete builder behavior is documented separately:

- image-related builders: [`docs/IMAGE_BUILDERS.md`](./docs/IMAGE_BUILDERS.md)
- filesystem-related builders: [`docs/ROOTFS_BUILDERS.md`](./docs/ROOTFS_BUILDERS.md)

For the architecture documents, start with [`docs/INDEX.md`](./docs/INDEX.md).

## Independence and Affiliation

This project is an independent personal open-source effort.
It is not affiliated with, derived from, or endorsed by Qualcomm or the Yocto Project.
