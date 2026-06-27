# JSON Graph Build Model

## Summary

`bobr` consumes one JSON DAG request and executes it entirely in Rust.

The input is a flat JSON envelope with:

- `schema`: format version, currently `"bobr-request-v1"`
- `store`: store root for the request
- `quiet` (optional)
- `jobs` (optional)
- `nodes`

`nodes` is a top-level object of recipe nodes keyed by technical ids. The
reserved id `root` identifies the build target for the current invocation.
`store` points at the store root for the request. Each node describes
either one builder node or one source node. Dependencies are encoded as id
references rather than inline child recipe objects.

The envelope fields are:

- `schema: string` (must be `"bobr-request-v1"`)
- `store: string`
- `quiet: bool`
- `jobs: integer`

Command-line arguments do not override the envelope; the JSON envelope is the
single source of build configuration.

Rust is responsible for:

- decoding the JSON graph
- validating each node against `InputSpec`
- computing `build_key`s
- performing top-down lookup planning
- building missing nodes bottom-up
- running ready nodes in parallel
- publishing current refs

There is no embedded Nickel runtime in `bobr`.

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

`Group` is the phony aggregate builder for requests that need one root but must
realize several otherwise unrelated targets. It does not merge or inspect input
payloads. It stages a constant zero-byte marker file after all inputs have been
realized, so its `RealizedObject` is only a completion marker. The meaningful
artifacts are the input targets and their normal publications.

`Source` is a separate execution class with this outer shape:

- `name`
- `tag = "Source"`
- `object_hash`
- optional `origin`

`Source` does not have `config` or `inputs`.

In v1, `Source` supports:

- `origin.tag = "Path"`
- `origin.path` must be an absolute host path
- `origin.unpack`, defaulting to `false`; when true, the local path is treated as a tar archive
- `origin.tag = "Http"`
- `origin.url` as one HTTP(S) URL or an ordered fallback list
- `origin.unpack`, defaulting to `false`
- `origin.archive_format` as an optional explicit unpack override
- `origin.tag = "OciRegistry"`
- `origin.image` as the registry image locator
- `origin.digest` as the pinned manifest or index digest
- `origin.platform` as the selected OCI platform for manifest lists and OCI
  indexes

If `origin` is omitted, the payload object must already exist in the store.
If the canonical object record is missing, Rust reconstructs it from the
declared object hash.

## Build Identity

Build keys, reuse keys, object hashes, and their relationship to store records
are defined by the store model. See [Store](./STORE.md#identity-model).

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

1. derives `build_key` from `object_hash`
2. checks `<store>/object-records/<object_hash>.json`
3. if the object record is absent, checks whether
   `<store>/objects/<object_hash>` already exists
4. if the object exists, reconstructs the missing canonical object record for
   `object_hash`
5. creates or repairs `<store>/builds/<build_key>` on a hit
6. if `origin` is missing and the object is absent, fails
7. if `origin.tag = "Path"` is present, materializes `origin.path` directly
8. if `origin.tag = "Http"` is present, downloads from `origin.url` in order
   and either stages one file object or unpacks one directory object
9. imports the staged object into `objects/<actual_hash>`
10. if `actual_hash != object_hash`, fails without writing the canonical
    object record or source build handle and reports the actual hash
11. otherwise writes the canonical object record for `object_hash` and creates
    or repairs `<store>/builds/<build_key>`

Execution then proceeds bottom-up:

- a missing node becomes ready when all of its direct dependencies are already
  reused or built
- ready nodes are submitted to the worker pool
- independent ready nodes may run in parallel
- a node is never built twice for the same `build_key`

The request is already a DAG-level representation rather than a fully nested
tree. The runtime still keeps planner and executor state keyed by `build_key`,
so identical graph fragments reuse the same internal state. `Source`
participates in the same planner/executor flow with a public `build_key` equal
to its declared `object_hash`.

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

`bobr [recipe.json]`

- if `recipe.json` is omitted, the JSON envelope is read from `stdin`
- `stdout`: JSON serialization of the realized root `RealizedObject`
- `stderr`: live progress log unless `quiet` is true
- `jobs`: limit parallel execution, default = available CPU parallelism
- `store`: set the store root for the request
- final store path: absolute path to an existing store root directory. The
  value is the store root itself; `bobr` does not add an implicit `.bobr/`
  directory.
