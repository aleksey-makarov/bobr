# Content-Addressed Store

## Summary

`mbuild` stores payloads as content-addressed objects, canonical realized
results as result records, and public build handles as symlink refs to those
results.

- `objects/` holds payloads addressed by `object_hash`
- `object-indexes/` holds derived leaf-hash indexes addressed by `object_hash`
- `results/` holds canonical result records addressed by `result_id`
- `reuses/` holds builder-only canonical reuse refs addressed by `reuse_key`
- `builds/` holds builder-only public build-handle refs addressed by `build_key`
- `result-refs/` holds human-facing refs from publication name to result record
- `object-refs/` holds human-facing refs from publication name to payload

Publication names do not participate in object identity, `build_key`, or
`reuse_key`. `result_id` is derived only from payload identity.
The store root is provided explicitly by `paths.store` in the JSON request
envelope. `mbuild` does not add an implicit `.mbuild/` directory.

## Layout

```text
<store>/
  objects/
    <object_hash>
  object-indexes/
    <object_hash>.jsonl
  reuses/
    <reuse_key> -> ../results/<result_id>.json
  builds/
    <build_key> -> ../results/<result_id>.json
  results/
    <result_id>.json
  result-refs/
    <name>.json -> ../results/<result_id>.json
  object-refs/
    <name> -> ../objects/<object_hash>
  logs/
    runs/
      <YYMMDDHHMMSS>-<pid>.jsonl
  builder-state/
    <builder>/
      logs/
```

`objects/<object_hash>` is the payload itself, either a file or a directory.
Concrete directory payload formats are builder-specific. For example, the
current image builders may realize image-related objects as OCI image layout
directories.

`object-indexes/<object_hash>.jsonl` is a derived cache. It is safe to delete:
mbuild can rebuild it from `objects/<object_hash>` when needed. The file has no
header or version line. Each line is one JSON object with:

- `path`: object-relative UTF-8 path
- `type`: `file` or `symlink`
- `hash`: fsobj node hash for that leaf

Directory entries are not stored in this index. Callers that need directory
hashes recompute them from their own tree structure and leaf hashes. For
fs-tree objects this structure is `manifest.jsonl`, which also accounts for
empty directories.

Generic CAS objects may contain non-UTF-8 filesystem names. Such objects can
still be imported and addressed by `object_hash`; mbuild simply skips the
human-readable JSONL index if a path cannot be represented as UTF-8. Fs-tree
objects are already UTF-8-only because their manifest paths and symlink targets
are JSON strings.

`results/<result_id>.json` stores one canonical realized result record.

Each result record contains:

- realized result identity: `result_id`
- payload identity: `object_hash`
- direct input identities under `inputs`, where each entry contains:
  - `object_hash`

`result_id` is derived only from `object_hash`, so different builder nodes can
share one result record when they intentionally stage the same payload. The
`Group` builder relies on this phony behavior: every `Group` stages the same
zero-byte marker. Its published root result is a completion marker, not an
authoritative manifest of the aggregate inputs.

`builds/<build_key>` stores the corresponding public build handle as a symlink
to the canonical result record. `reuses/<reuse_key>` stores the canonical
builder reuse index. The language-level realized result is `RealizedResult`.
For builders it may also carry `build_key`; for `Source` it does not.

## Result Reuse

For one planned builder node, the runtime tries reuse in this order:

1. build-handle hit on `build_key`
2. canonical reuse hit on `reuse_key`
3. actual builder execution

If a canonical builder result exists but the public build handle is missing,
`mbuild`
recreates the missing build-handle ref and reuses the result.

For `Source`, there is no `build_key` and no `reuse_key`.

The runtime tries reuse in this order:

1. canonical result hit on `result_id`
2. existing object hit on `object_hash`
3. actual source materialization

If source materialization produces a different object than the declared
`object_hash`, the actual object is still imported into `objects/`, but the
canonical `results/<result_id>.json` record is not written and the build
fails with the actual hash.

## Publication

Every recipe node carries a publication name.

After the runtime reuses or builds a node, it updates:

- `result-refs/<name>.json -> ../results/<result_id>.json`
- `object-refs/<name> -> ../objects/<object_hash>`

This `object-refs/` rule is the same for every object kind. Filesystem tree
objects still store their payload as an object directory containing
`manifest.jsonl` and `root/`; `root/` is part of the object layout, not the
publication symlink target.

If the current publication name already points at a different result, the old
current refs are rotated into timestamp-suffixed history refs.

## Logging

Each `mbuild` invocation writes:

- one structured event log under `<store>/logs/runs/<YYMMDDHHMMSS>-<pid>.jsonl`
- raw builder logs under `<store>/builder-state/<builder>/logs/<name>/`

The event log records lifecycle events such as:

- `start`
- `cache-hit`
- `result-hit`
- `cache-miss`
- `run`
- `publish`
- `done`
- `fail`
