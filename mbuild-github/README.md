# mbuild-github

`mbuild-github` is the GitHub source builder backend for `mbuild`.

It implements `mbuild-core::Builder` for recipes with `type = "github"` and manages:
- mirror repositories in `.mbuild/github/mirrors/`
- materialized source trees in `.mbuild/materialized/`
- builder-private state in `.mbuild/github/internal.ncl`
- shared state in `.mbuild/state.ncl`

## Supported Verbs

- `build`:
  - ensures the mirror contains the requested commit
  - materializes the corresponding source tree into `.mbuild/materialized/<artifact>`
- `cache`:
  - ensures/updates the mirror for the requested commit
  - does not materialize output

## Recipe Shape

Current GitHub recipe fields:
- `type = "github"`
- `repo` (GitHub URL)
- `commit` (40-char lowercase hex)
- optional `inputs`, `outputs` (validated by `mbuild`)

`mbuild-github` expects recipe values already selected by artifact key from `.mbuild/recipes.ncl`.

## Notes

- The builder currently relies on `git` and `tar` executables.
- Mirror naming format is `owner_repo.git`.
- This crate is a library backend, not a standalone CLI tool.
