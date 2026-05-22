# JSON Graph Build Model

## Summary

`mbuild` consumes one JSON DAG request and executes it entirely in Rust.

The input is a JSON envelope with:

- `paths`
- `options` (optional)
- `nodes`

`nodes` is a top-level object of recipe nodes keyed by technical ids. The
reserved id `root` identifies the build target for the current invocation.
`paths.store` points at the store root for the request. Each node describes
either one builder node or one source node. Dependencies are encoded as id
references rather than inline child recipe objects.

`options` currently supports:

- `quiet: bool`
- `jobs: integer`

Explicit CLI flags override `options`.

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

There are two recipe node classes.

Builder nodes have this outer shape:

- `name`
- `tag`
- `config`
- `inputs`

`tag` selects one registered builder. `config` is opaque builder payload.
`inputs` is an object keyed by named input dependencies.

Input values are encoded generically:

- every present input value is one node id string
- optional inputs are omitted entirely
- ordered extra inputs are expressed by sortable names such as `in000`,
  `in001`, ...

The runtime rejects:

- unknown builder tags
- missing required inputs
- extra inputs for builders that do not allow them
- non-string input values

Children are always referenced by node id.

`Source` is a separate execution class with this outer shape:

- `name`
- `tag = "Source"`
- `object_hash`
- optional `origin`

`Source` does not have `config` or `inputs`.

In v1, `Source` supports:

- `origin.type = "path"`
- `origin.mode = "direct" | "tar"`
- `origin.path` must be a non-empty relative path without `..`
- `origin.type = "http"`
- `origin.url` as one HTTP(S) URL or an ordered fallback list
- `origin.unpack`, defaulting to `false`
- `origin.archive_format` as an optional explicit unpack override
- `origin.type = "oci-registry"`
- `origin.image` as the registry image locator
- `origin.digest` as the pinned manifest or index digest
- manifest lists and OCI indexes resolve to the `linux/amd64` manifest only

If `origin` is omitted, the payload object must already exist in the store.
If the canonical result record is missing, Rust reconstructs it from the
declared object hash.

## Build Identity

For one builder node, `build_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `build_key`s

Dependency order follows the builder input contract:

- reserved inputs in spec order
- extra inputs in lexical name order

It does not follow the order of fields in JSON.

`reuse_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency input identities

Each direct input identity contains:

- `object_hash`

`result_id` is computed from:

- `object_hash`

`build_key` is the builder invocation identity.
`reuse_key` is the builder-only canonical reuse identity that can be computed
before execution.
`result_id` is the realized result identity shared by both builders and
sources.

## Planning and Execution

Planning starts at node `root`.

For each builder node, Rust:

1. computes `build_key`
2. checks `<store>/builds/<build_key>`
3. if that misses, checks the canonical builder reuse record by `reuse_key`
4. only if both miss, recurses into direct dependencies

This is the top-down phase. It determines the minimal missing subgraph needed
to realize the root.

For each `Source` node, Rust:

1. computes `result_id` from `object_hash`
2. checks `<store>/results/<result_id>.json`
3. if `origin` is missing and the result record is absent, checks whether
   `<store>/objects/<object_hash>` already exists
4. if the object exists, reconstructs the missing canonical result record from
   `object_hash`
5. if `origin.type = "path"` is present, resolves `origin.path` against
   `paths.local`
6. if `origin.type = "http"` is present, downloads from `origin.url` in order
   and either stages one file object or unpacks one directory object
7. imports the staged object into `objects/<actual_hash>`
8. if `actual_hash != object_hash`, fails without writing the canonical
   result record and reports the actual hash
9. otherwise writes the canonical result record for `object_hash`

Execution then proceeds bottom-up:

- a missing node becomes ready when all of its direct dependencies are already
  reused or built
- ready nodes are submitted to the worker pool
- independent ready nodes may run in parallel
- a node is never built twice for the same `build_key`

The request is already a DAG-level representation rather than a fully nested
tree. The runtime still keeps planner and executor state keyed by `build_key`,
so identical builder graph fragments reuse the same internal state. `Source`
participates in the same planner/executor flow, but does not have a public
`build_key`.

## Builder Interface

Rust builders still receive:

- builder config payload
- resolved input payload paths

They do not receive unresolved recipe nodes.

Concrete object formats are builder-specific. The current OCI import and
extraction contracts are described in [`IMAGE_BUILDERS.md`](./IMAGE_BUILDERS.md).
The current filesystem composition builder contract is described in
[`ROOTFS_BUILDERS.md`](./ROOTFS_BUILDERS.md).

Builders may use the realized payload content of resolved inputs. Input
validation is builder-specific and is based on named input semantics plus
payload inspection.

Builder semantics depend only on:

- builder tag
- builder config
- realized payload content of direct inputs

## CLI Contract

`mbuild [recipe.json]`

- if `recipe.json` is omitted, the JSON envelope is read from `stdin`
- `stdout`: JSON serialization of the realized root `RealizedResult`
- `stderr`: live progress log unless `--quiet`
- `--jobs/-j`: limit parallel execution, default = available CPU parallelism
- `paths.store`: absolute path to an existing store root directory
- `paths.local`: optional absolute path to an existing local-source root
  directory; required only for `Source` path origins
