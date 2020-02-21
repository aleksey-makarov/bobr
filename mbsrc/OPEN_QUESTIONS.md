# mbsrc Open Questions

This file tracks unresolved design decisions for the MVP and near-term evolution.

## Decided (for now)

- Runtime paths are not configurable in MVP.
- `mbsrc` uses `./.mbuild/` as root for recipes/state/storage.
- In this repository, `mbsrc/.mbuild/` is treated as local runtime data and is gitignored.
- Default recipes file is `./.mbuild/recipes.ncl`.
- Shared state file is `./.mbuild/state.ncl`.
- Internal state file is `./.mbuild/github/internal.ncl`.
- `materialized/` and `state.ncl` are initialized by default.
- `mirrors/` is private and created lazily by `build`.
- Current `mbsrc` view of `recipes.ncl` is a top-level map: `artifact-name -> recipe`.
- Artifact names are case-sensitive.
- Recipe uses fixed GitHub commit (`source.type`, `source.repo`, `source.commit`).
- Smoke tests for `build/materialize/dematerialize` command flow are implemented.

## Open Questions

1. Transition to output hashing (future)
- `materialize` currently uses `artifact-name` as identifier.
- Define migration path to content-based output hash identifiers later.

2. Shared state schema
- Define long-term schema for `.mbuild/state.ncl`.
- Define compatibility policy when schema evolves.

3. Internal state schema
- Define strict boundary between private bookkeeping and shared/public fields.
- Define migration policy for `.mbuild/github/internal.ncl`.

4. Reconcile policy between state and filesystem
- Define how and when `.mbuild/github/internal.ncl` should be reconciled with real contents of `.mbuild/github/mirrors/`.

5. Read-only policy details
- Which exact file permission mode to apply across files/directories.
- Whether to support a future override for writable materialization (currently no).

6. Future config model (post-MVP)
- If/when runtime paths become configurable, define precedence (CLI flag vs config file vs env).
