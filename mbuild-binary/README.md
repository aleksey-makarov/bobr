# mbuild-binary

`mbuild-binary` is the binary build backend for `mbuild`.

It implements `mbuild-core::Builder` for recipes with `type = "binary"` and executes build scripts inside a short-lived Podman container.

## Supported Verbs

- `build`:
  - validates inputs and outputs
  - creates empty output directories in `.mbuild/materialized/`
  - runs the recipe script in a container with network disabled

No custom verbs are currently defined.

## Recipe Shape

Current binary recipe fields:
- `type = "binary"`
- optional `inputs` (array of artifact names)
- optional `outputs` (array of artifact names)
- `script` (must start with shebang)

Inputs and outputs are mounted by name:
- `/in/<name>` for inputs
- `/out/<name>` for outputs

## Runtime Notes

- Uses `podman run --rm --network=none`.
- Runs as host user (`--userns=keep-id`, explicit `uid:gid`).
- Uses a pinned default image:
  - `docker.io/library/gcc@sha256:99732c3fbda294e6e7c8bb463a98ec394d48de16ee45fece6f28d7bf7d9dbd99`
- Requires `podman` on PATH.

This crate is a library backend, not a standalone CLI tool.
