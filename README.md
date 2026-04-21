# mbuild

`mbuild` executes one JSON DAG request.

The entry file is a JSON document whose top-level object is a table of recipe
nodes keyed by technical ids. The root build target is the entry with the
reserved id `root`. Dependencies are encoded as id references in input slots.
`mbuild` parses that DAG request, validates each node against the registered
`BuilderSpec`, computes build keys in Rust, performs top-down store lookups,
and builds only the missing nodes. Missing leaves and other ready nodes may
execute in parallel.

Every recipe node still uses one generic shape:

```json
{
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
```

Node payload fields:

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

`mbuild build` defaults to `./.mbuild/recipe.json`. On success it prints the
realized root `Build` as JSON to `stdout`. Live progress goes to `stderr`. Use
`--quiet` to suppress progress output. Use `--jobs/-j` to limit parallel
builder execution; the default is the available CPU parallelism.

The store layout is content-addressed:

- `.mbuild/objects/` stores payload objects by `object_hash`
- `.mbuild/results/` stores canonical realized results by `result_key`
- `.mbuild/builds/` stores public build handles by `build_key`
- `.mbuild/meta-refs/` and `.mbuild/object-refs/` store published current refs

`build_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `build_key`s

`result_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency input identities

Each direct input identity contains:

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
