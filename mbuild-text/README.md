# mbuild-text

`mbuild-text` is a backend for publishing text files as artifacts.

It implements `mbuild-core::Builder` for recipes with `type = "text"`.

## Purpose

This builder is used to turn files from the recipes directory into regular artifacts
stored in `.mbuild/objects`, with metadata and refs like other builders.

A primary use case is build-script artifacts (`artifact_kind = "build-script"`).

## Recipe Shape

- `type = "text"`
- `artifact_kind = "..."`
- optional `outputs = [ ... ]`
- exactly one of:
  - `source = "relative/path"` (single source)
  - `sources = { "output-name" = "relative/path", ... }`

`source`/`sources` paths must be relative to `.mbuild/` and must not contain `..`.

## Build Result

For each output:
- object payload directory: `.mbuild/objects/<id>/`
- metadata: `.mbuild/meta/<id>.ncl`
- ref symlink: `.mbuild/refs/<name> -> ../meta/<id>.ncl`

Current `id` is equal to output name.
