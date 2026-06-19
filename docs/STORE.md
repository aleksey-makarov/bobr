# Store

## Summary

The store contains immutable payloads, canonical object records,
mutable build and publication refs, and per-run operational logs.

The store is a content-addressed store (CAS). This means payload identity is
derived from normalized content, not from the path or publication name used to
reach it. Importing the same payload content produces the same `object_hash`;
different payload content produces a different address. Human-facing names and
build handles are mutable refs layered on top of immutable content-addressed
objects and canonical object records.

## Identity Model

Every planned node has a `build_key`.

For `Source`, `build_key` is the declared `object_hash` reinterpreted as a
build key. Source nodes have no planned inputs.

For a builder node, `build_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `build_key`s

Dependency order follows the builder input contract:

- reserved inputs in spec order
- extra inputs in lexical name order

It does not follow the order of fields in JSON.

`build_key` identifies a planned graph node and backs the public build-handle
ref. For builder nodes, a build-handle hit can be used before looking through
the node's direct inputs.

After a builder's direct inputs are realized, the builder-only canonical reuse
identity, `reuse_key`, is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `object_hash` values

The same builder input contract order is used for these `object_hash` values.

`reuse_key` is independent from the particular dependency invocations that
produced those input objects. Different build graph fragments can therefore
reuse one canonical builder object when their direct input payload identities
match.

Executing a builder or materializing a source produces one payload object. The
payload is addressed by `object_hash`.

The same `object_hash` also keys the canonical object record for that payload.
Different builder nodes can share one object record when they intentionally
stage the same payload.

Publication names do not participate in object identity, `build_key`, or
`reuse_key`. The language-level realized object is `RealizedObject`; it carries
the `build_key` that resolved to the object when that key is known.

## Reuse Model

For one planned builder node, builder reuse lookup uses this order:

1. build-handle hit on `build_key`
2. canonical reuse hit on `reuse_key`
3. actual builder execution

If a canonical builder object exists but the public build handle is missing,
the missing build-handle ref is recreated and the object is reused.

For `Source`, there is a `build_key` but no `reuse_key`.

Source reuse lookup uses this order:

1. canonical object-record hit on `object_hash`
2. existing object hit on `object_hash`
3. actual source materialization

On a source hit or successful materialization, the store creates or repairs the
source build handle `builds/<object_hash>`.

If source materialization produces a different object than the declared
`object_hash`, the actual object is still imported into `objects/`, but the
canonical `object-records/<object_hash>.json` record and source build handle are
not written, and the source import fails with the actual hash.

## Store Layout

The filesystem layout mirrors the identity model:

```text
<store>/
  objects/
    <object_hash>
  reuses/
    <reuse_key> -> ../object-records/<object_hash>.json
  builds/
    <build_key> -> ../object-records/<object_hash>.json
  object-records/
    <object_hash>.json
  object-record-refs/
    <name>.json -> ../object-records/<object_hash>.json
  object-refs/
    <name> -> ../objects/<object_hash>
  fs-files/
    ...
  fs-trees/
    <manifest-object-hash>/
  logs/
    <YYMMDDhhmmss>[.N]/
      events.jsonl
      index.jsonl
      <00000000>-<tag>[-<name>]/
        meta.json
        events.jsonl
        raw/
  tmp/
    <YYMMDDhhmmss>[.N]/
      <00000000>-<tag>[-<name>]/
```

- `objects/` holds payloads addressed by `object_hash`.
- `object-records/` holds canonical object records addressed by `object_hash`.
- `reuses/` holds builder-only canonical reuse refs addressed by `reuse_key`.
- `builds/` holds public build-handle refs addressed by `build_key`.
- `object-record-refs/` holds human-facing refs from publication name to object
  record.
- `object-refs/` holds human-facing refs from publication name to payload.
- `fs-files/` holds regular-file payloads referenced by fs-tree manifest v2
  objects.
- `fs-trees/` caches materialized filesystem roots for fs-tree manifest v2
  objects.

`objects/<object_hash>` is the payload itself, either a file or a directory.
Concrete directory payload formats are builder-specific. For example, the
OCI registry source handler realizes imported images as OCI image layout
directories.

Filesystem tree builder results store a canonical fs-tree manifest v2 text file
as the object payload. Regular file entries in that manifest reference payloads
stored under `fs-files/`. Materialized roots under `fs-trees/` are cache
entries created on demand for builders that need a filesystem root path.

Generic CAS objects may contain non-UTF-8 filesystem names. Such objects can
still be imported and addressed by `object_hash`. Fs-tree objects are
UTF-8-only because their manifest paths and symlink targets are JSON strings.

`object-records/<object_hash>.json` stores one canonical object record. The
record payload contains:

- payload identity: `object_hash`
- direct input identities under `inputs`, where each entry contains:
  - `object_hash`

`builds/<build_key>` stores the corresponding public build handle as a symlink
to the canonical object record. `reuses/<reuse_key>` stores the canonical
builder reuse index.

`logs/<run-id>/<serial>-<tag>[-<name>]/raw/` stores raw per-subject log files
such as captured tool output. `tmp/<run-id>/<serial>-<tag>[-<name>]/` is the
matching per-subject scratch directory. Scratch directories may be removed or
quarantined after execution; they are not part of the log record.

## Publication

Every recipe node carries a publication name.

After a node is reused or built, the current publication refs are updated:

- `object-record-refs/<name>.json -> ../object-records/<object_hash>.json`
- `object-refs/<name> -> ../objects/<object_hash>`

This `object-refs/` rule is the same for every object kind. Filesystem tree
builder results store the manifest itself as the object payload. The
publication symlink never points directly at `fs-files/` or at a materialized
`fs-trees/` cache directory.

If the current publication name already points at a different object, the old
current refs are rotated into timestamp-suffixed history refs.

## Logging

Each run writes:

- one run-level structured event log under `<store>/logs/<run-id>/events.jsonl`
- one workspace index under `<store>/logs/<run-id>/index.jsonl`
- per-subject logs under
  `<store>/logs/<run-id>/<00000000>-<tag>[-<name>]/`

`run-id` uses the local `<YYMMDDhhmmss>` timestamp. If another run has already
claimed that directory, `.1`, `.2`, and so on are appended. Each builder,
source, or scheduler subject gets a store-allocated serial number for its log
directory name. The serial is an internal allocation detail; the full original
tag, recipe name, subject key, and workspace paths are stored in that subject's
`meta.json`. Subject keys are build keys; for source subjects that build key is
the declared object hash.

The run-level event log records lifecycle events such as:

- `start`
- `cache-hit`
- `object-hit`
- `cache-miss`
- `run`
- `publish`
- `done`
- `fail`

Subject events are also written to the subject's own `events.jsonl`. Raw logs
created by builders are written under the subject's `raw/` directory.
