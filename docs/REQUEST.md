# Request

`bobr` reads a JSON document (described below) from standard input or from a
file named on the command line. The document is a **request**: it describes how
to build an object. `bobr` builds that object and prints its `ObjectHash` to
standard output. For the model behind requests — objects, recipes, keys — see
[Concepts](./CONCEPTS.md).

The request is a single JSON object:

```json
{
  "schema": "bobr-request-v1",
  "store": "/abs/path/to/store",
  "quiet": false,
  "jobs": 8,
  "nodes": {
    "root": { "...": "..." }
  }
}
```

- `schema` — format version; must be `"bobr-request-v1"`
- `store` — the store root for this request: an absolute path to an existing
  directory (see [Store](./STORE.md))
- `quiet` — optional bool; suppress the live progress log
- `jobs` — optional integer; limit on parallel builder execution
- `nodes` — the recipe DAG

The recipe DAG is a JSON object: each member's value is a recipe. The required
key `root` holds the recipe to build; the others hold the recipes it depends on.

<!--
## Recipe nodes

There are two node classes: **builder** nodes and **source** nodes. Both carry a
`name` and a `tag`; dependencies are always referenced by node id.
-->

### Source nodes

A source node has this shape:

```json
{
  "name": "linux-src",
  "tag": "Source",
  "object_hash": "0123…abcd",
  "origin": {
    "tag": "Http",
    "url": ["https://example.invalid/linux.tar.xz"],
    "unpack": true
  }
}
```

A source node has no `config` and no `inputs`, and its `BuildKey` is its declared
`object_hash`. It supports three origins:

- **`Path`** — `origin.path` is an absolute host path; `origin.unpack` (default
  `false`) treats it as a tar archive when true.
- **`Http`** — `origin.url` is one HTTP(S) URL or an ordered fallback list;
  `origin.unpack` (default `false`); `origin.archive_format` may override archive
  detection for unpacked sources.
- **`OciRegistry`** — `origin.image` is the registry image locator,
  `origin.digest` the pinned manifest or index digest, and `origin.platform`
  selects the platform when the digest names a manifest list or OCI index.

A source may also omit `origin`. Then the object must already exist in the store
under its `object_hash`; `bobr` reconstructs the canonical object record from the
declared hash if it is missing.

If a source materializes a different object than its declared `object_hash`,
`bobr` still imports the actual object (under its real hash) but does not record
it under the declared hash; the source fails, reporting the actual hash so the
recipe can be corrected and rerun without downloading again.

### Builder nodes

A builder node has this shape:

```json
{
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
```

- `name` — publication name
- `tag` — builder tag from the Rust builder registry
- `config` — opaque builder payload
- `inputs` — object keyed by named input dependencies

Inputs are encoded generically:

- every present input value is one node id string
- optional inputs are omitted entirely
- ordered extra inputs use sortable names such as `in000`, `in001`, …

The runtime rejects:

- unknown builder tags
- missing required inputs
- extra inputs for builders that do not allow them
- non-string input values

#### `Group`

`Group` is the aggregate builder for requests that need one `root` but must
realize several otherwise unrelated targets. It has empty config and one or more
arbitrary inputs:

```json
{
  "name": "all-targets",
  "tag": "Group",
  "config": {},
  "inputs": { "in000": "toolchain", "in001": "rootfs", "in002": "image" }
}
```

`Group` does not merge or inspect its inputs; it stages a constant zero-byte
marker once all inputs are realized, so its object is only a completion marker.
The meaningful artifacts are the input targets themselves.

## Higher-level recipe tags

Nickel recipes may use higher-level synthetic tags that are lowered before
`bobr` sees the request. The package-aware helpers `Autotools`, `Makefile`,
`Meson`, `PerlModule`, and `SandboxBuild` inject a generated build rootfs and
lower to Rust-side `Sandbox` nodes. The explicit-rootfs variants are
`AutotoolsRootfs`, `MakefileRootfs`, `MesonRootfs`, `PerlModuleRootfs`, and
`SandboxBuildRootfs`.

## See also

- Concrete builder behavior: [OCI image inputs](./IMAGE_BUILDERS.md) and
  [filesystem builders](./ROOTFS_BUILDERS.md).
- The store layout, the `BuildKey`/`ReuseKey` computation, and refs:
  [Store](./STORE.md).
