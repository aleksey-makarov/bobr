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

A recipe for the `Source` builder has this shape:

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

- `name` — a human-facing name for the result
- `tag` — must be `"Source"`
- `object_hash` — the `ObjectHash` this source must produce
- `origin` — how to obtain the object this recipe describes; defined below

A recipe for the `Source` builder may also omit `origin`. Then the object must
already exist in the store under its `object_hash`, and `bobr` reuses it; if it
does not, the source fails.

A recipe for any other builder has this shape:

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

- `name` — a human-facing name for the result
- `tag` — the name of the builder that builds this recipe
- `config` — the builder's configuration; its shape is defined by the builder
- `inputs` — dependencies keyed by input name; each value is the key of another
  member of `nodes` — the recipe this one depends on

Inputs are encoded generically:

- every present input value is one node id string
- optional inputs are omitted entirely
- ordered extra inputs use sortable names such as `in000`, `in001`, …

The runtime rejects:

- unknown builder tags
- missing required inputs
- extra inputs for builders that do not allow them
- non-string input values

<!-- Дальше не смотри -- будем считать что дальше мы ещё не сделали -->

<!-- Дальше -- описание origins -->

- **`Path`** — `origin.path` is an absolute host path; `origin.unpack` (default
  `false`) treats it as a tar archive when true.
- **`Http`** — `origin.url` is one HTTP(S) URL or an ordered fallback list;
  `origin.unpack` (default `false`); `origin.archive_format` may override archive
  detection for unpacked sources.
- **`OciRegistry`** — `origin.image` is the registry image locator,
  `origin.digest` the pinned manifest or index digest, and `origin.platform`
  selects the platform when the digest names a manifest list or OCI index.

<!-- Дальше -- описание известных builders -->
