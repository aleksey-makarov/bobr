# Content-Addressed Store

## Summary

`mbuild` stores payloads as content-addressed objects and stores realized
builder results as build records.

- `objects/` holds payloads addressed by `object_hash`.
- `builds/` holds build records addressed by `build_key`.
- `meta-refs/` holds human-facing symlinks from published name to build record.
- `object-refs/` holds human-facing symlinks from published name to payload
  object.

Object identity depends only on payload content. Publication names do not
participate in object identity or build-record identity.

## Layout

```text
.mbuild/
  objects/
    <object_hash>
  builds/
    <build_key>.json
  meta-refs/
    <name>.json -> ../builds/<build_key>.json
  object-refs/
    <name> -> ../objects/<object_hash>
  logs/
    runs/
      <YYMMDDHHMMSS>-<pid>.jsonl
  .. builder-specific files and dirs ..
```

`objects/<object_hash>` is the payload itself, either a file or a directory.

`builds/<build_key>.json` stores one realized `Build` value. The language-level
`Build` value and the on-disk build record have the same shape.

Internal CAS identifiers use bare lowercase hex:

- `object_hash` is a 64-character lowercase hex string
- `build_key` is a 64-character lowercase hex string
- algorithm-qualified strings such as `sha256:...` remain only for external
  digests and declared fetch hashes

`meta-refs/<name>.json` is a human-facing symlink to the selected current build
record. Historical generations are kept as timestamp-suffixed refs such as
`meta-refs/<name>.<YYMMDDHHMMSS>.json`.

`object-refs/<name>` is a human-facing symlink to the selected current payload
object. Historical generations mirror metadata refs under names such as
`object-refs/<name>.<YYMMDDHHMMSS>`.

Each builder owns a same-named subdirectory under `.mbuild/` for builder-
specific runtime state, temporary files, logs, and caches.

`logs/runs/<YYMMDDHHMMSS>-<pid>.jsonl` is the event log for one `mbuild`
invocation.

Builder raw logs live under:

```text
.mbuild/builder-state/<builder>/logs/<name>/<YYMMDDHHMMSS>-<short-build-key>-<label>.log
```

These raw logs contain captured stdout/stderr for external builder commands.
For example, `binary` writes raw logs for `podman run`, and `image` writes raw
logs for `podman import`, `podman create`, `podman cp`, `podman commit`, and
`podman inspect`.

## Image Layering Semantics

`Image` consumes one optional base `container-image` and one or more
`binary-output` directories.

Two modes exist:

- `bootstrap`: build a new image from scratch from the supplied
  `binary-output` directories
- `layered`: start from a base `container-image` and apply the supplied
  `binary-output` directories on top

Layering is conflict-aware.

Rules:

- directory over directory merge is allowed
- any attempt to replace an existing non-directory path is a conflict
- any file-vs-file replacement is a conflict
- any file-vs-directory or directory-vs-file replacement is a conflict
- any symlink-vs-existing-path replacement is a conflict

As a consequence, `Image` never silently overwrites files coming from the base
image or from earlier `binary-output` inputs. Path conflicts fail the build.

This prevents accidental construction of hybrid images where runtime components
from different systems are mixed by implicit overwrite.

## Object Identity

`object_hash` is the hash of payload content only.

Hashing rules:

- algorithm: `sha256`
- root object kind: `file` or `directory`
- for regular files, the hash includes:
  - file bytes
  - executable bit only
- for directories, the hash includes:
  - relative paths
  - entry kinds: file, directory, symlink
  - executable bit for regular files
  - file content digests for files
  - symlink target bytes for symlinks
- directory traversal order is strict lexicographic order by relative path

The hash excludes:

- uid, gid
- mtime, ctime, atime
- xattrs and ACLs
- inode or device data
- symlink mode
- publication names
- authored recipe metadata
- builder provenance metadata
- builder attrs

Consequences:

- identical payloads built in different temp directories have the same
  `object_hash`
- identical payloads produced by different builders are the same object
- one object may be published under many names

## Build Records

Build records are stored at:

```text
.mbuild/builds/<build_key>.json
```

A build record is the canonical realized result of one builder invocation.
Language-level `Build` values have this exact shape.

Example shape:

```json
{
  "schema": "mbuild-build-v2",
  "build_key": "0123456789abcdef...",
  "created_at": "2026-03-24T12:34:56.123456789Z",
  "object_hash": "fedcba9876543210...",
  "kind": "build-script|source-tree|fetched-file|binary-output|container-image|...",
  "producer": {
    "builder": "text|fetch|binary|image|container-image"
  },
  "input_build_keys": [
    "89abcdef01234567..."
  ],
  "attrs": {}
}
```

Rules:

- each build record is keyed by `build_key`
- `build_key` is computed before builder execution from:
  - builder tag
  - normalized payload
  - ordered `input_build_keys`
- `build_key` does not depend on `object_hash`
- `build_key` does not depend on `created_at`
- a build record points at exactly one `object_hash`
- multiple build records may point at the same object
- `created_at` records the first materialization time of that build record in
  RFC3339 UTC format
- builder-generated semantic metadata lives in the build record
- downstream builder calls consume `Build` values, not raw store paths

`Build` includes machine-facing semantic data such as:

- `build_key`
- `object_hash`
- `kind`
- `attrs`
- optionally `producer` and `input_build_keys` if they are exposed by the
  language

`object_path` is a runtime detail. It is not part of the language-level `Build`
value.

## Publication Refs

### Metadata refs

Stored at:

```text
.mbuild/meta-refs/<name>.json -> ../builds/<build_key>.json
.mbuild/meta-refs/<name>.<YYMMDDHHMMSS>.json -> ../builds/<old_build_key>.json
```

Purpose:

- human-facing lookup from published name to build record
- stable publication of one selected name or alias
- convenient inspection of the currently published realized result

These refs are publication state:

- they are not part of object identity
- they are not part of build-record identity
- builders do not read them directly
- removing them does not invalidate objects or build records

### Object refs

Stored at:

```text
.mbuild/object-refs/<name> -> ../objects/<object_hash>
.mbuild/object-refs/<name>.<YYMMDDHHMMSS> -> ../objects/<old_object_hash>
```

Purpose:

- human-friendly direct access to payloads
- convenient inspection of the object currently published under a name

These refs are human-facing only:

- they are not part of object identity
- builders do not read them directly
- removing them must not affect store semantics

## Runtime Responsibilities

The store does not define dependency semantics. Dependency structure comes from
the Nickel STORE program interpreted by Rust.

`mbuild` loads one recipe entry file, evaluates it to a top-level STORE action,
and interprets the resulting action tree. STORE recursion is expressed in
Nickel through `bind`, not through recursive store lookups by name.

For one primitive builder action, the interpreter:

1. receives a builder action whose dependency fields are already realized
   `Build` values
2. validates builder-specific input kinds and required attrs
3. collects ordered `input_build_keys`
4. computes `build_key`
5. reuses an existing build record on matching `build_key`
6. executes the appropriate Rust builder on cache miss
7. stores the produced payload in `objects/`
8. writes or reuses one build record in `builds/`, including `created_at`
9. updates `meta-refs/<name>.json` and `object-refs/<name>`
10. if the published name already pointed at a different build, rotates the old
    current refs into timestamp-suffixed generation refs
11. returns the realized `Build` value

Rust builders do not receive publication names as part of build semantics.
Names are consumed only by the interpreter for implicit publication.

Rust-builder semantics are defined only by builder config and by the payload
content of already-realized input objects. Dependency metadata carried by
`Build` values is visible to Nickel for inspection, but it is not a semantic
input to Rust builders unless Nickel explicitly copies it into a downstream
builder payload. A builder that changes behavior because dependency metadata
differs, while payload objects and builder config stay the same, violates the
store model.

## Monadic Execution Example

Conceptually, a recipe may evaluate to a STORE program like:

```nickel
store.bind (store.fetch "bash-src-5.3" { ... }) (fun bashSrc =>
store.bind (store.text "buildscript-bash-stage2" { ... }) (fun bashScript =>
store.bind (store.container_image "bootstrap-image" { ... }) (fun bootstrapImage =>
store.binary "bash-stage2" { optimize = "size" } bootstrapImage bashScript [bashSrc])))
```

Execution alternates between Nickel and Rust:

1. Nickel evaluates the entry file to the first STORE action.
2. Rust interprets that action.
3. If the action is `Bind`, Rust interprets the left side, obtains a Nickel
   value, applies the continuation inside Nickel, and gets the next STORE
   action.
4. When Rust encounters a primitive builder action, it performs the build/store
   steps listed above and returns a realized `Build` record back to Nickel.
5. Nickel code may inspect `Build` metadata before constructing the next action.

This is how dependency recursion is expressed without giving Rust builders
access to authored recipe metadata or to human-facing refs.

## Builder Data Model

Rust builders consume:

- builder-specific configuration
- resolved input payload paths
- resolved input `Build` records for semantic validation

For the `binary` builder specifically:

- the first `sources` entry is the primary source tree
- additional `sources` entries may be either source trees or fetched files
- auxiliary fetched files are exposed to the build script under `/in/sourcesN`

Rust builders produce:

- a payload that becomes one object
- builder-generated semantic metadata that becomes part of `Build`

The interpreter writes the build record and updates both publication ref
namespaces.

## Logging And Progress

`mbuild` emits two kinds of logs:

- an **event log**, which is the structured JSONL record of one `mbuild` run
- **raw logs**, which are the captured stdout/stderr logs of external commands

The event log records lifecycle events such as:

- `start`
- `cache-hit`
- `cache-miss`
- `run`
- `publish`
- `done`
- `fail`

Each event entry stores the builder tag, published name, shortened
`build_key`, a short message, and optional data such as shortened
`object_hash`, `raw_log_path`, and structured details. Full identifiers remain
available in `details`.

By default, `mbuild` also prints concise live progress lines to `stderr` while
the run is in progress. The final `Build` summary remains on `stdout`.

`mbuild --quiet` disables live progress output but still writes both the event
log and all raw logs.

## Container Image Objects

A `container-image` object is a file object whose contents are a JSON
descriptor, for example:

```json
{
  "schema": "mbuild-container-image-object-v1",
  "storage": "external-podman",
  "image_ref": "docker.io/...@sha256:...",
  "image_digest": "sha256:..."
}
```

The descriptor file is hashed like any other file object. The corresponding
`Build` record carries the semantic type and builder-generated metadata for that
object.

## Builder-Specific Conventions

### `text`

- output is usually a file object
- executable mode for `build-script` participates in object hashing
- build attrs may include fields such as `source_bytes`

### `fetch`

- downloaded blob cache lives in `.mbuild/fetch/cache`
- unpacked or raw result becomes an object
- build attrs carry source URL, declared hash, unpack flag, archive format, and
  normalized-root information

### `binary`

- runtime state lives in `.mbuild/binary/`
- output staging lives in `.mbuild/binary/tmp`
- run logs live in `.mbuild/binary/logs`
- one builder call produces one `Build` result
- build attrs carry stable install-related data needed by downstream image
  assembly

### `image` and `container-image`

- output payload is a file object containing the image descriptor
- build metadata carries semantic type and provenance
- publication names do not affect `build_key`
