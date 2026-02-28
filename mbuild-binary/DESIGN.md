# mbuild-binary Design (Current State)

This document describes current technical behavior of `mbuild-binary`.

## Role

`mbuild-binary` is a backend library crate used by `mbuild`.
It implements `mbuild_core::Builder` for recipes with `type = "binary"`.

## Builder Interface

`mbuild-binary` provides:
- `get_type() -> "binary"`
- `run_build(artifact, recipe_value)`
- `summarize_recipe(recipe_value)`

No custom verbs are currently exposed.

## Recipe Contract

Expected fields (after selection by artifact key from `.mbuild/recipes.ncl`):
- `type = "binary"`
- `script: String` (must start with `#!`)
- `inputs?: [String]`
- `outputs?: [String]`

Name validation (`inputs`, `outputs`):
- non-empty
- not `.` / `..`
- allowed chars: `[A-Za-z0-9._-]`

If `outputs` is omitted, one output is published with current artifact name.

## Storage Model

Shared storage root: `.mbuild/`

- object payloads: `.mbuild/objects/<id>/`
- metadata: `.mbuild/meta/<id>.ncl`
- name refs: `.mbuild/refs/<name>` -> `../meta/<id>.ncl`

Current `id` is equal to output artifact name.

## Build Flow

`run_build` does:

1. Parse and validate recipe.
2. Ensure `.mbuild/{objects,meta,refs}` exist.
3. Resolve every input name via `.mbuild/refs/<name>` to object directory.
4. Create temporary output root under `.mbuild/.tmp-binary-...`.
5. Write script to temporary executable file on host.
6. Run one-shot Podman container.
7. On success, publish each output:
   - move output directory to `.mbuild/objects/<id>`
   - write `.mbuild/meta/<id>.ncl`
   - update `.mbuild/refs/<name>` symlink
8. Cleanup temporary script and temporary output root.

Build success criterion: container exits with code `0` and every declared output directory exists.

## Container Runtime Contract

Container command:
- `podman run --rm`
- `--network=none`
- `--userns=keep-id`
- `--user <uid>:<gid>`

Mounts:
- inputs: `<object_path>:/in/<name>:O`
- outputs: `<tmp_output_path>:/out/<name>:rw`
- script: `<tmp_script>:/__mbuild_binary_script:ro`

Entrypoint command:
- `/__mbuild_binary_script`

Default image:
- `localhost/mbuild-binary:bookworm-toolchain`

Image is expected to be built from `mbuild-binary/Containerfile` (base `buildpack-deps:bookworm`).

## Error Mapping

Internal errors map to `BuilderError`:
- invalid recipe/contract -> `InvalidRecipe`
- input resolution, container runtime, filesystem/publish failures -> `ExecutionFailed`

`mbuild` renders these as `error[builder-failed]: ...`.

## Known Gaps

- No custom verbs yet.
- No semantic validation of output contents.
- Ref target is interpreted by file name (`<id>.ncl`); metadata body is not parsed yet for policy checks.
