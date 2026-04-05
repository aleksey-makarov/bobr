# mbuild

`mbuild` executes one JSON recipe graph.

The entry file is a JSON document whose root object describes one build target.
Dependencies are embedded inline as child recipe objects. `mbuild` parses that
nested tree, validates each node against the registered `BuilderSpec`, computes
build keys in Rust, performs top-down store lookups, and builds only the
missing nodes. Missing leaves and other ready nodes may execute in parallel.

Every recipe node uses one generic shape:

```json
{
  "name": "tar-1.35",
  "tag": "Binary",
  "config": {
    "kind": "binary-output"
  },
  "inputs": {
    "image": { "...": "recipe object" },
    "script": { "...": "recipe object" },
    "sources": [{ "...": "recipe object" }]
  }
}
```

Node fields:

- `name`: publication name
- `tag`: builder tag from the Rust builder registry
- `config`: opaque builder payload
- `inputs`: object keyed by input slot names from the selected `BuilderSpec`

Input encoding is generic and follows the slot arity declared by the builder:

- `One`: one inline recipe object
- `Optional`: always present, either `null` or one inline recipe object
- `Many`: an array of inline recipe objects

The runtime rejects:

- unknown builder tags
- extra input slots
- missing declared slots
- wrong input arity for a slot

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

The dependency order comes from `BuilderSpec.inputs`, not from JSON field order.
This lets `mbuild` keep the general runtime independent from concrete builders.

Concrete builder behavior is documented separately. For the current image
builder path, see [`docs/IMAGE_BUILDERS.md`](./docs/IMAGE_BUILDERS.md).

For the architecture documents, start with [`docs/INDEX.md`](./docs/INDEX.md).
