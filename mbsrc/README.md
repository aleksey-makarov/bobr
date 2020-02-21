# mbsrc MVP Notes

`mbsrc` is a minimal source-builder utility for the `mbuild` project.
It is implemented in Rust and currently targets GitHub repositories only.

## Scope (Current MVP)

- Output hashing is not implemented yet.
- Recipes are read from `./.mbuild/recipes.ncl` (Nickel).
- `recipes.ncl` is a shared file for all builders; in current MVP, `mbsrc` reads a top-level map of GitHub source recipes.
- Two separated operations:
  - `build`: fetch/update repository mirrors and verify required commit exists.
  - `materialize`: expand a selected source output into a configured directory.
- One reverse operation:
  - `dematerialize`: remove a previously materialized output.
- Interface bootstrap:
  - `./.mbuild/materialized/` and `./.mbuild/state.ncl` are ensured by default.
  - `./.mbuild/github/mirrors/` is created lazily on first `build`.

## Config Model (MVP)

Current `mbsrc` view of `./.mbuild/recipes.ncl` is a top-level record (object) where:

- key = artifact name (case-sensitive)
- value = recipe for that artifact

Example Nickel config:

```nickel
{
  zlib = {
    source = {
      type = "github",
      repo = "https://github.com/madler/zlib.git",
      commit = "0123456789abcdef0123456789abcdef01234567",
    },
  },
}
```

Recipe rules:

- `source.type` must be `github`.
- `source.repo` must be a GitHub repo URL.
- `source.commit` must be a 40-char lowercase hex commit hash.
- No local filesystem paths inside recipe.

No submodules, no LFS, no patching, and no dynamic version functions in this MVP.

## Suggested CLI

- `mbsrc build <artifact-name>`
- `mbsrc materialize <artifact-name>`
- `mbsrc dematerialize <artifact-name>`

Error format:
- failures are printed as `error[<class>]: <message>` for stable grouping in scripts/logs.

`materialize` / `dematerialize` are preferred names to keep a clear contract with future builders.

## Storage Layout (Per Current Working Directory)

`mbsrc` state and storage are grouped under `./.mbuild/`:

- `recipes.ncl`:
  - shared recipes file for builders (public input).
- `state.ncl`:
  - shared/public state, initialized automatically if missing.
- `github/internal.ncl`:
  - private runtime state for implementation bookkeeping.
- `github/mirrors/`:
  - bare mirror clones (`git clone --mirror`) for tracked repositories, created on demand.
- `materialized/`:
  - expanded read-only source trees addressable by `artifact-name`.

Example shape:

```text
<cwd>/
  .mbuild/
    recipes.ncl
    state.ncl
    github/
      internal.ncl
      mirrors/
        owner_repo.git/
    materialized/
      <artifact-name>/
```

Repository note:
- In this repository, `mbsrc/.mbuild/` is gitignored because it is local runtime data.

## Build Behavior

Algorithm:

1. Load and evaluate `./.mbuild/recipes.ncl` via embedded Nickel runtime.
2. Select recipe by `<artifact-name>` key.
3. Validate recipe fields.
4. Resolve mirror path as `.mbuild/github/mirrors/owner_repo.git`.
5. Ensure `.mbuild/github/mirrors/` exists.
6. If mirror exists:
   - check whether required commit is already present;
   - if missing, run `git fetch --all --prune`.
7. If mirror does not exist:
   - run `git clone --mirror`.
8. Verify required commit exists with `git cat-file -e <commit>^{commit}`.
9. Write/update both `.mbuild/github/internal.ncl` and `.mbuild/state.ncl`.

`build` succeeds only when the exact recipe commit is present in local mirror.

## Materialize Behavior

- Resolve requested output by artifact name from local state and mirror data.
- Expand source tree from mirror into `.mbuild/materialized/<artifact-name>/`.
- Keep output read-only.
- Record materialization in state.
- Current implementation uses `git archive` + `tar` and does not expose `.git` in output.

## Dematerialize Behavior

- Remove `.mbuild/materialized/<artifact-name>/`.
- Clear related materialization metadata in `state.ncl`.
- Keep mirror data intact.
- If materialized files are read-only, implementation makes them writable first and then removes them.

## Design Intent

This MVP is intentionally small: reliable local mirrors + explicit materialization lifecycle.
Hash-based identity, richer metadata, and broader source kinds are deferred.
