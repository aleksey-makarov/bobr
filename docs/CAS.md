# Content-Addressed Store

## Summary

`mbuild` stores payloads as content-addressed objects, canonical realized
results as result records, and public build handles as symlink refs to those
results.

- `objects/` holds payloads addressed by `object_hash`
- `results/` holds canonical result records addressed by `result_id`
- `reuses/` holds builder-only canonical reuse refs addressed by `reuse_key`
- `builds/` holds builder-only public build-handle refs addressed by `build_key`
- `meta-refs/` holds human-facing refs from publication name to result record
- `object-refs/` holds human-facing refs from publication name to payload

Publication names do not participate in object identity, `build_key`, or
`reuse_key`. `result_id` is derived only from payload and metadata identity.
The store root is provided explicitly by `paths.store` in the JSON request
envelope. `mbuild` does not add an implicit `.mbuild/` directory.

## Layout

```text
<store>/
  objects/
    <object_hash>
  reuses/
    <reuse_key> -> ../results/<result_id>.json
  builds/
    <build_key> -> ../results/<result_id>.json
  results/
    <result_id>.json
  meta-refs/
    <name>.json -> ../results/<result_id>.json
  object-refs/
    <name> -> ../objects/<object_hash>
    <fs-tree-name> -> ../objects/<object_hash>/root
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

`results/<result_id>.json` stores one canonical realized result record.

Each result record contains:

- realized result identity: `result_id`
- payload identity: `object_hash`
- metadata identity: `meta_hash`
- result metadata under `meta`
- direct input identities under `inputs`, where each entry contains:
  - `object_hash`
  - `meta_hash`

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

- `meta-refs/<name>.json -> ../results/<result_id>.json`
- `object-refs/<name> -> ../objects/<object_hash>` for ordinary file and
  directory objects
- `object-refs/<name> -> ../objects/<object_hash>/root` for filesystem tree
  objects

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
