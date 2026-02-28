# mbuild-binary

`mbuild-binary` is the containerized binary build backend for `mbuild`.

It implements `mbuild-core::Builder` for recipes with `type = "binary"` and executes scripts in short-lived Podman containers.

## Supported Verbs

- `build`:
  - resolves all declared `inputs` from `.mbuild/refs/<name>`
  - mounts resolved object directories to `/in/<name>`
  - mounts temporary output directories to `/out/<name>`
  - runs either inline `script` or `/in/<build-script>/script.sh` in container
  - publishes declared outputs into object storage:
    - `.mbuild/objects/<id>/`
    - `.mbuild/meta/<id>.ncl`
    - `.mbuild/refs/<name>`

No custom verbs are currently defined.

## Recipe Shape

Current binary recipe fields:
- `type = "binary"`
- optional `inputs` (`[String]`)
- optional `outputs` (`[String]`)
- optional `script` (must start with shebang)

If `outputs` is omitted, builder publishes one output with the current artifact name.
If `script` is omitted, recipe must provide exactly one input with `artifact_kind = "build-script"`
and exactly one input with `artifact_kind = "source-tree"`.

## Runtime Notes

- Uses `podman run --rm --network=none`.
- Runs as host user (`--userns=keep-id`, explicit `uid:gid`).
- Uses default image:
  - `localhost/mbuild-binary:bookworm-toolchain`
- Build this image from `mbuild-binary/Containerfile`:
  - `podman build -t localhost/mbuild-binary:bookworm-toolchain -f mbuild-binary/Containerfile .`
- Requires `podman` on PATH.

This crate is a library backend, not a standalone CLI tool.
