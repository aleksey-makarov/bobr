# mbuild-binary Design (Current State)

This document describes the current technical behavior of `mbuild-binary`.

## Role In The System

`mbuild-binary` is a library backend crate used by `mbuild`.
It implements `mbuild_core::Builder` for recipes with `type = "binary"`.

There is no standalone CLI in this crate anymore.
Command parsing, artifact lookup, and high-level orchestration are done by `mbuild`.

## Builder Interface

`mbuild-binary` provides:

- `get_type() -> "binary"`
- `run_build(artifact, recipe_value)`
- `summarize_recipe(recipe_value)`

It does not expose custom verbs yet (only universal `build`).

## Recipe Contract

Expected recipe fields (after selection by artifact key in `.mbuild/recipes.ncl`):

- `type = "binary"`
- `script: String` (must start with `#!`)
- `inputs?: [String]`
- `outputs?: [String]`

Name validation for `inputs` and `outputs`:

- non-empty
- not `.` or `..`
- allowed chars: `[A-Za-z0-9._-]`

## Workspace Layout

`mbuild-binary` uses shared workspace root:

- `.mbuild/materialized/` as both input source and output destination

Input/output mapping:

- input `<name>` -> host `.mbuild/materialized/<name>` -> container `/in/<name>`
- output `<name>` -> host `.mbuild/materialized/<name>` -> container `/out/<name>`

## Build Execution Flow

`run_build` performs:

1. Parse and validate binary recipe from `serde_json::Value`.
2. Ensure `.mbuild/` and `.mbuild/materialized/` directories exist.
3. Verify every declared input directory exists in materialized storage.
4. Recreate every declared output directory as empty.
5. Write recipe script into a temporary executable file.
6. Run one-shot container build with Podman.
7. Remove temporary script file.

Success criterion: container command exits with code `0`.

## Container Runtime Contract

Container command:

- `podman run --rm`
- `--network=none`
- `--userns=keep-id`
- `--user <uid>:<gid>`

Mounts:

- inputs: `--volume <host>:/in/<name>:O`
- outputs: `--volume <host>:/out/<name>:rw`
- script: `--volume <tmp_script>:/__mbuild_binary_script:ro`

Entrypoint command in container:

- `/__mbuild_binary_script`

Current default image (hardcoded):

- `docker.io/library/gcc@sha256:99732c3fbda294e6e7c8bb463a98ec394d48de16ee45fece6f28d7bf7d9dbd99`

## Error Mapping

Internal runtime errors are mapped to `BuilderError`:

- recipe shape/validation issues -> `InvalidRecipe`
- container/process/filesystem/runtime failures -> `ExecutionFailed`

In `mbuild`, `ExecutionFailed` is reported as `error[builder-failed]: ...`.

## Current Limitations

- No custom verbs (for example, no binary-specific `cache`).
- No explicit image field in recipe yet.
- No semantic validation of output contents beyond successful process exit.
- Requires working `podman` on host runtime path.
