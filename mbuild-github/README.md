# mbuild-github

`mbuild-github` is the GitHub source backend for `mbuild`.

It implements `mbuild-core::Builder` for recipes with `type = "github"` and manages:
- mirror repositories in `.mbuild/github/mirrors/`
- published source objects in `.mbuild/objects/<id>/`
- object metadata in `.mbuild/meta/<id>.ncl`
- artifact refs in `.mbuild/refs/<name>` (symlink to `../objects/<id>`)

`id` is currently equal to output artifact name.

## Supported Verbs

- `build`:
  - ensures mirror contains the requested revision
  - publishes source tree as object + metadata + ref for each declared output
  - if `outputs` is omitted, falls back to current artifact name
- `cache`:
  - ensures/updates mirror for the requested revision
  - does not publish outputs

## Recipe Shape

Current GitHub recipe fields:
- `type = "github"`
- `owner` (GitHub owner/org)
- `repo` (repository name)
- `rev` (40-char lowercase commit hash)
- optional `outputs` (`[String]`)

`mbuild-github` receives the already selected recipe value from `.mbuild/recipes.ncl`.

## Notes

- Relies on host `git` and `tar` executables.
- Mirror naming format is `owner_repo.git`.
- This crate is a library backend, not a standalone CLI tool.
