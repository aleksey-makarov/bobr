# JSON Graph Build Model

## Summary

`mbuild` consumes one JSON DAG request and executes it entirely in Rust.

The input file is a top-level object of recipe nodes keyed by technical ids.
The reserved id `root` identifies the build target for the current invocation.
Each node describes one builder node in a generic format driven by the Rust
builder registry. Dependencies are encoded as id references rather than inline
child recipe objects.

Rust is responsible for:

- decoding the JSON graph
- validating each node against `BuilderSpec`
- computing `build_key`s
- performing top-down lookup planning
- building missing nodes bottom-up
- running ready nodes in parallel
- publishing current refs

There is no embedded Nickel runtime in `mbuild`.

## Recipe Model

Every node has the same outer shape:

- `name`
- `tag`
- `config`
- `inputs`

`tag` selects one registered builder. `config` is opaque builder payload.
`inputs` is an object keyed by slot names from `BuilderSpec.inputs`.

Input values are encoded by declared slot arity:

- `One`: one inline recipe object
- `Optional`: always present, either `null` or one inline recipe object
- `Many`: an array of inline recipe objects

The runtime rejects:

- unknown builder tags
- extra input slots
- missing declared slots
- input values that do not match the declared arity

Children are always referenced by node id.

## Build Identity

For one recipe node, `build_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `build_key`s

Dependency order follows `BuilderSpec.inputs` order, not the order of fields in
JSON.

`result_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency input identities

Each direct input identity contains:

- `object_hash`
- `meta_hash`

`build_key` is the public identity of a node in the dependency graph.
`result_key` is the canonical identity of one realized result payload.

## Planning and Execution

Planning starts at node `root`.

For each node, Rust:

1. computes `build_key`
2. checks `.mbuild/builds/<build_key>`
3. if that misses, checks the canonical result by `result_key`
4. only if both miss, recurses into direct dependencies

This is the top-down phase. It determines the minimal missing subgraph needed
to realize the root.

Execution then proceeds bottom-up:

- a missing node becomes ready when all of its direct dependencies are already
  reused or built
- ready nodes are submitted to the worker pool
- independent ready nodes may run in parallel
- a node is never built twice for the same `build_key`

The request is already a DAG-level representation rather than a fully nested
tree. The runtime still keeps planner and executor state keyed by `build_key`,
so identical graph fragments reuse the same internal state.

## Builder Interface

Rust builders still receive:

- builder config payload
- resolved input payload paths
- resolved input metadata records

They do not receive unresolved recipe nodes.

Concrete object formats are builder-specific. The current image-related builder
contracts are described in [`IMAGE_BUILDERS.md`](./IMAGE_BUILDERS.md). The
current filesystem composition builder contract is described in
[`ROOTFS_BUILDERS.md`](./ROOTFS_BUILDERS.md).

Builders may use both the realized payload content and the resolved input
metadata they receive. Input validation is builder-specific and is based on
slot semantics plus payload inspection.

Builder semantics depend only on:

- builder tag
- builder config
- realized payload content of direct inputs
- resolved metadata of direct inputs

Direct input metadata also participates in canonical result identity through
direct input `meta_hash` values.

## CLI Contract

`mbuild build [recipe.json]`

- default input path: `./.mbuild/recipe.json`
- `stdout`: JSON serialization of the realized root `Build`
- `stderr`: live progress log unless `--quiet`
- `--jobs/-j`: limit parallel execution, default = available CPU parallelism
