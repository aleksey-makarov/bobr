# mbuild

`mbuild` is the top-level CLI that orchestrates multiple builder backends.

At the current stage:
- recipes are read from `.mbuild/recipes.ncl`
- each artifact has a `type`
- `mbuild` routes execution to the registered builder for that type

## CLI Model

Primary form:
- `mbuild <artifact> [verb]`
- default verb is `build`

Introspection commands:
- `mbuild info <artifact>`
- `mbuild verbs <type-or-artifact>`

Compatibility alias:
- `mbuild build <artifact>`

## Verbs

- `build` is universal (supported by all builders)
- additional verbs are builder-defined

Current examples:
- for `github`: `build`, `cache`
- for `binary`: `build`
- for `text`: `build`

## Current Backends

- `mbuild-github`: GitHub source backend
- `mbuild-binary`: containerized binary build backend
- `mbuild-text`: text artifact backend

## Notes

- Current name refs (`.mbuild/refs/<name>`) point to object payloads.
- Future TODO: add separate metadata refs namespace (for example, `refs-meta`) for metadata-only artifacts.

## Artifact Kind Contract

Current storage/runtime invariants by `artifact_kind`:

- `build-script`:
  - payload at `.mbuild/objects/<id>` must be a **file**
  - `binary` builder mounts this file as `/__mbuild_binary_script` and executes it
- `source-tree`:
  - payload at `.mbuild/objects/<id>` must be a **directory**
  - `binary` builder mounts it as `/in/<name>`

`binary` validates these invariants before starting a container build.
