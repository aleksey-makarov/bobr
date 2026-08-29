# Store

## Summary

The store contains immutable payloads, user-facing object metadata, per-run
operational logs, the `BuildKey` → `ObjectHash` and `ReuseKey` → `ObjectHash`
reuse mappings, and mutable refs from a name to its object. It is a
content-addressed store (CAS): payload identity is derived from normalized
content, not from the path or name used to reach it.

The store root is the directory named by the request's `store` field — an
absolute path to an existing directory, used as-is (`bobr` adds no implicit
`.bobr/` layer).

For the mental model behind objects, keys, and CAS, see
[Concepts](./CONCEPTS.md). This document is the precise reference for the store's
identity model, reuse rules, and on-disk layout.

## Identity Model

[Concepts](./CONCEPTS.md) introduces the three identities (`object_hash`,
`build_key`, `reuse_key`); this section is their normative definition.

Every recipe has a `build_key`.

For `Source`, `build_key` is the declared `object_hash` reinterpreted as a
build key. Source recipes have no inputs.

For a builder recipe, `build_key` is computed from:

- builder tag
- normalized config payload
- ordered direct dependency `build_key`s

Dependency order follows the builder input contract:

- reserved inputs in spec order
- extra inputs in lexical name order

It does not follow the order of fields in JSON.

`build_key` identifies a recipe; the store maps it to the object the recipe
produced (`builds/<build_key>`). For a builder recipe, a hit on that mapping can
be used before looking through the recipe's direct inputs.

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

The same `object_hash` also keys a user-facing object metadata record for that
payload. Different builder recipes can share one record when they intentionally
stage the same payload. The record is not part of object identity or cache
resolution.

Recipe names do not participate in object identity, `build_key`, or
`reuse_key`. The language-level realized object is `RealizedObject`; it carries
the `build_key` that resolved to the object when that key is known.

## Reuse Model

For a builder recipe, the working-store lookup starts with an exact
`build_key` hit in `builds/`. A complete exact hit skips everything below it.
On an exact miss, bobr resolves possible `object_hash` identities of the direct
inputs and computes the corresponding `reuse_key` values. It then checks those
keys in `reuses/`. This identity resolution may happen before input content has
been copied into the working store. Only when exact and reuse lookup both miss
does bobr realize complete direct inputs and execute the builder.

If a reuse hit provides an object but the current `build_key` mapping is
missing, bobr publishes that mapping and reuses the object. A builder can also
publish every reuse key in the current candidate set that resolves to the chosen
object.

For `Source`, there is a `build_key` but no `reuse_key`.

Source realization asks whether its declared `object_hash` is complete in the
working store, then whether configured content sources can provide it. If not,
the Source origin is materialized. Object records are not read during this
lookup. On a source hit or successful materialization, bobr writes user-facing
metadata and creates or repairs the source's `builds/<object_hash>` mapping.

If source materialization produces a different object than the declared
`object_hash`, the actual object is still imported into `objects/`, but no
object metadata or source `builds/<object_hash>` mapping is written, and the
source import fails with the actual hash.

### Local repository capabilities

A request can additionally name local repositories (see
[Request](./REQUEST.md#local-repositories)). Each repository is one concrete
read-only backend from which bobr derives separate capabilities:

- a **trusted index** answers `BuildKey` and `ReuseKey` queries with candidate
  `ObjectHash` values; it supplies identity, not object bytes;
- a **content source** supplies an object's bytes by `ObjectHash`. The current
  implementation can hardlink objects and every referenced fs-file, or copy
  both ordinary objects and complete fs-tree closures into independent
  working-store inodes. Fs-file copy preserves and verifies logical ownership,
  mode, timestamp, and content identity before atomic publication. Hardlink
  repositories require the repository and working-store `objects/` directories
  to share a filesystem, as must their `fs-files/` directories; bobr validates
  the two pairs independently.

Every repository provides the content-source capability. A repository with
`trusted = true` additionally provides the trusted-index capability; with
`trusted = false`, its mappings are not exposed to the resolver at all.

The Realizer consults working-store mappings before repository mappings, and
checks working-store content before secondary content. A secondary mapping can
therefore be useful before its object is imported locally. Both adapters retain
the same `LocalRepository` backend and its shared validated read-only content
reader; they cannot be constructed directly from unrelated store handles.
Mapping lookup never opens object records. Remote capabilities are not
implemented yet.

## Store Layout

The filesystem layout mirrors the identity model:

```text
<store>/
  objects/
    <object_hash>
  reuses/
    <reuse_key> -> ../objects/<object_hash>
  builds/
    <build_key> -> ../objects/<object_hash>
  object-records/
    <object_hash>.json
  object-refs/
    <name> -> ../objects/<object_hash>
  fs-files/
    ...
  fs-trees/
    <manifest-object-hash>/
  fs-tree-refs/
    <name> -> ../fs-trees/<manifest-object-hash>
```

A run's log and work directories are **not** part of this layout: the request
names them, and the caller creates them (see [Request](./REQUEST.md)). By
convention they live under the store — `<store>/logs/<run-id>` and
`<store>/work/<run-id>`, which is what `bobr-build.sh` does by default — and the
work directory has to be on the store's filesystem, since build output is
published out of it by renaming and hardlinking. Their contents are:

```text
<logs>/
  events.jsonl
  index.jsonl
  <00000000>-<tag>[-<name>]/
    meta.json
    events.jsonl
    raw/
<work>/
  <00000000>-<tag>[-<name>]/
```

- `objects/` holds payloads addressed by `object_hash`.
- `object-records/` holds user-facing metadata records addressed by
  `object_hash`.
- `reuses/` maps a `reuse_key` to its object (builder recipes only).
- `builds/` maps a `build_key` to its object.
- `object-refs/` holds human-facing refs from recipe name to the latest
  successful object for that name.
- `fs-files/` holds regular-file payloads referenced by fs-tree manifest
  objects.
- `fs-trees/` caches materialized filesystem roots for fs-tree manifest
  objects.
- `fs-tree-refs/` holds human-facing refs from recipe name to the latest
  materialized filesystem root for that name.

`objects/<object_hash>` is the payload itself, either a file or a directory.
Concrete directory payload formats are builder-specific. For example, the
OCI registry source handler realizes imported images as OCI image layout
directories.

Filesystem tree builder results store a canonical fs-tree manifest text file
as the object payload. Regular file entries in that manifest reference payloads
stored under `fs-files/`. Materialized roots under `fs-trees/` are cache
entries created on demand for builders that need a filesystem root path.
When a named builder input asks for a filesystem root, the runtime also updates
`fs-tree-refs/<name> -> ../fs-trees/<manifest-object-hash>`. These refs are
for inspection only. Runtime lookup uses object identity and the
`fs-trees/` cache directly; it does not read `fs-tree-refs/`. If the same name
is later materialized from a different manifest object, the old current ref is
rotated into an mtime-suffixed generation ref before the new current ref is
installed.

Generic CAS objects may contain non-UTF-8 filesystem names. Such objects can
still be imported and addressed by `object_hash`. Fs-tree objects are
UTF-8-only because their manifest paths and symlink targets are JSON strings.

`object-records/<object_hash>.json` is user-facing metadata for people and
store-inspection tools. Its current schema is `bobr-object-record-v4`; it
contains:

- `object_hash` — the object the record describes
- `build_key` and `inputs` — producer information supplied by the first
  successful writer of this record
- `run_id` — optional; the store run that recorded it

The record is idempotent: an existing record is retained rather than rewritten.
Consequently its producer fields can be neutral metadata written while recording
an already-present or imported object; they are not authoritative provenance for
every mapping that later reaches this object.

`builds/<build_key>` and `reuses/<reuse_key>` are canonical symlinks whose
targets encode an `object_hash` as `../objects/<object_hash>`. Mapping lookup
validates and reads the hash from the symlink text without following its target
or inspecting object content; content is checked separately when realization
needs it. Publication creates these mappings only after the target object is
complete in the store. Object records remain independent inspection metadata,
just as `object-refs/` and `fs-tree-refs/` are human-facing views rather than
lookup inputs.

`<logs>/<serial>-<tag>[-<name>]/raw/` stores raw per-subject log files such as
captured tool output. `<work>/<serial>-<tag>[-<name>]/` is the matching
per-subject scratch directory. Scratch directories are removed after execution on
a best-effort basis; cleanup failures are logged as warnings and the scratch
directory is left in place.

## Object Refs

Every recipe carries a name. The name enters no identity computation — not
`object_hash`, `build_key`, or `reuse_key`; it is used only to create the refs
described here.

When a named recipe successfully resolves to an object, the current object ref
is updated:

- `object-refs/<name> -> ../objects/<object_hash>`

This `object-refs/` rule is the same for every object kind. Filesystem tree
builder results store the manifest itself as the object payload. The
object ref never points directly at `fs-files/` or at a materialized
`fs-trees/` cache directory. When an object has been successfully published,
its user-facing metadata is normally available as
`object-records/<object_hash>.json`; object-ref lookup does not depend on that
metadata.

Unlike `object-refs/`, `fs-tree-refs/` are inspection aids only, created when a
filesystem root is materialized for a named input.

If the current object ref already points at a different object, the old current
ref is rotated into a timestamp-suffixed history ref.

## Logging

Each run writes:

- one run-level structured event log under `<logs>/events.jsonl`
- one workspace index under `<logs>/index.jsonl`
- per-subject logs under `<logs>/<00000000>-<tag>[-<name>]/`

The run id comes from the request; `bobr-build.sh` derives it from the local
`<YYMMDDhhmmss>` timestamp and appends `.1`, `.2`, and so on when a directory of
that name is already taken. Each builder,
source, or scheduler subject gets a store-allocated serial number for its log
directory name. The serial is an internal allocation detail; the full original
tag, recipe name, subject key, and workspace paths are stored in that subject's
`meta.json`. Subject keys are build keys; for source subjects that build key is
the declared object hash.

The run-level event log is the full audit log of the run, and subject events
are also written to each subject's own `events.jsonl`. Raw logs created by
builders are written under the subject's `raw/` directory. The event record
format, the closed `status` vocabulary, and the format guarantees are specified
in [Build logging](LOGGING.md).
