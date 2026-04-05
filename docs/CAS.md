# Content-Addressed Store

## Summary

`mbuild` stores payloads as content-addressed objects, canonical realized
results as result records, and public build handles as symlink refs to those
results.

- `objects/` holds payloads addressed by `object_hash`
- `results/` holds canonical result records addressed by `result_key`
- `builds/` holds public build-handle refs addressed by `build_key`
- `meta-refs/` holds human-facing refs from publication name to build handle
- `object-refs/` holds human-facing refs from publication name to payload

Publication names do not participate in object identity, `build_key`, or
`result_key`.

## Layout

```text
.mbuild/
  objects/
    <object_hash>
  builds/
    <build_key> -> ../results/<result_key>.json
  results/
    <result_key>.json
  meta-refs/
    <name>.json -> ../builds/<build_key>
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
current image builders may realize `container-image` objects as OCI image
layout directories.

`results/<result_key>.json` stores one canonical realized result record.

Each result record contains:

- payload identity: `object_hash`
- metadata identity: `meta_hash`
- result metadata under `meta`
- direct input identities under `inputs`, where each entry contains:
  - `object_hash`
  - `meta_hash`

`builds/<build_key>` stores the corresponding public build handle as a symlink
to the canonical result record. The language-level `Build` value exposes
`build_key`, not `result_key`.

## Result Reuse

For one planned node, the runtime tries reuse in this order:

1. build-handle hit on `build_key`
2. canonical result hit on `result_key`
3. actual builder execution

If a canonical result exists but the public build handle is missing, `mbuild`
recreates the missing build-handle ref and reuses the result.

## Publication

Every recipe node carries a publication name.

After the runtime reuses or builds a node, it updates:

- `meta-refs/<name>.json -> ../builds/<build_key>`
- `object-refs/<name> -> ../objects/<object_hash>`

If the current publication name already points at a different result, the old
current refs are rotated into timestamp-suffixed history refs.

## Logging

Each `mbuild` invocation writes:

- one structured event log under `.mbuild/logs/runs/<YYMMDDHHMMSS>-<pid>.jsonl`
- raw builder logs under `.mbuild/builder-state/<builder>/logs/<name>/`

The event log records lifecycle events such as:

- `start`
- `cache-hit`
- `result-hit`
- `cache-miss`
- `run`
- `publish`
- `done`
- `fail`
