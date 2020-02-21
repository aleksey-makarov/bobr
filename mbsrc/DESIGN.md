# mbsrc Design (Agreed Decisions)

This document captures decisions explicitly agreed for `mbsrc`.

## 1. Purpose

`mbsrc` is a source-builder utility for the `mbuild` project.
Current scope is minimal: maintain GitHub mirrors and prepare for explicit materialization lifecycle.

## 2. Command Model

Current command set:

- `mbsrc build <artifact-name>`
- `mbsrc materialize <artifact-name>`
- `mbsrc dematerialize <artifact-name>`

Notes:

- `build` and `materialize` are separate operations.
- `dematerialize` removes only materialized data and keeps mirrors.
- CLI errors are emitted in stable grouped format: `error[<class>]: <message>`.

## 3. Config Source

- Default recipes file is `./.mbuild/recipes.ncl`.
- No extra config flags for path overrides in MVP.
- `recipes.ncl` is shared for all builders; current `mbsrc` reads its own recipe map from it.

## 4. Config Structure

`./.mbuild/recipes.ncl` exports a top-level map:

- key: artifact name (case-sensitive)
- value: recipe for that artifact

Example shape:

```nickel
{
  artifact_name = {
    source = {
      type = "github",
      repo = "https://github.com/owner/repo.git",
      commit = "0123456789abcdef0123456789abcdef01234567",
    },
  },
}
```

## 5. Recipe Contract (MVP)

- `source.type` must be `github`.
- `source.repo` must be a GitHub URL.
- `source.commit` must be a fixed 40-char lowercase hex commit hash.
- Recipe does not include local filesystem paths.

## 6. Build Contract

For `build <artifact-name>`:

1. Load `./.mbuild/recipes.ncl` via embedded Nickel runtime.
2. Resolve recipe by artifact key.
3. Ensure mirror at `.mbuild/github/mirrors/owner_repo.git`:
   - clone mirror if absent;
   - fetch if present and commit missing.
4. Verify commit exists in mirror.
5. Update runtime state files.

## 7. Runtime Layout

All mbsrc runtime files are under `./.mbuild/`:

- `recipes.ncl` (shared recipes input)
- `state.ncl` (shared/public state)
- `github/internal.ncl` (private/internal state)
- `github/mirrors/`
- `materialized/`
- In this repository, `mbsrc/.mbuild/` is gitignored as local runtime state.

Bootstrap policy:

- `materialized/` and `state.ncl` are part of builder interface and are initialized by default.
- `mirrors/` is private implementation detail and is created lazily by `build`.

## 8. Materialization Model

- Materialized outputs are intended to be read-only.
- Materialization identity is currently based on `artifact-name`.
- Shared state (`state.ncl`) tracks materialization metadata for artifacts.
- Output hashing itself is not implemented yet and is deferred.

## 9. Deferred Topics

- Output content hashing implementation.
- Extended provenance.
- Configurable paths.
- Multi-source kinds beyond GitHub.
- Orchestrator integration.
