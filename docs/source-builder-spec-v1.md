# Source Builder Spec v1

This document defines `Recipe v1` and `Output v1` for a fixed-version git source builder.

## Scope

- Builder kind: `source.git`.
- No logical recipe inputs.
- One logical output: `source_tree`.
- Output content is exactly the repository tree at a fixed commit.

## Recipe v1

Required fields:

- `schema_version`: must be `source.recipe.v1`.
- `type`: must be `source.git`.
- `name`: human-readable package name.
- `source.url`: git URL.
- `source.commit`: full commit hash.
- `source.object_format`: `sha1` or `sha256` (git object format).
- `checkout.submodules`: `true` or `false`.
- `checkout.lfs`: `true` or `false`.
- `checkout.include_git_dir`: `true` or `false`.
- `outputs.source_tree.kind`: must be `dir`.
- `outputs.source_tree.hash.kind`: must be `git_tree`.
- `outputs.source_tree.hash.value`: expected git tree hash.

Optional fields:

- `description`: free text.
- `license`: SPDX identifier or project-specific string.
- `provenance_policy`: policy hint for stored provenance.

## Output v1

Produced output map:

- `source_tree`:
  - `kind`: `dir`
  - `hash.kind`: `git_tree`
  - `hash.value`: resolved tree hash of checked out commit
  - `materialization.format`: `directory`
  - `meta.commit`: commit used for checkout
  - `meta.url`: source URL

Notes:

- `hash.value` is the git tree object ID, not an archive checksum.
- If `include_git_dir = false`, materialized output excludes `.git`.
- Submodules and LFS behavior is controlled only by `checkout`.

## Validation Rules

1. `source.commit` must be a full hash compatible with `source.object_format`.
2. Resolved commit tree hash must equal `outputs.source_tree.hash.value`.
3. If `checkout.submodules = true`, submodule content must be present in the output tree.
4. If `checkout.lfs = true`, LFS pointers must be materialized to file content.
5. Builder must fail with a deterministic error class:
   - `not_found`
   - `auth_required`
   - `network_transient`
   - `hash_mismatch`
   - `unsupported_source`

## Provenance Minimum

Store provenance with at least:

- `recipe_digest`
- `builder_id`
- `builder_version`
- `source.url`
- `source.commit`
- `source.tree`
- `checkout` options
- `started_at`
- `finished_at`
- `logs_ref`

## Non-Goals in v1

- Dynamic recipe generation by package version function.
- Multi-repository composition.
- Patch application in source builder.
- Alternate source kinds (archive/file) in the same recipe schema.
