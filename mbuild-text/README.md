# mbuild-text

`mbuild-text` is a backend for publishing text payloads as artifacts.

It implements `mbuild-core::Builder` for recipes with `type = "text"`.

## Purpose

This builder publishes text from recipe values into regular artifacts
stored in `.mbuild/objects`, with metadata and refs like other builders.

A primary use case is build-script artifacts (`artifact_kind = "build-script"`).

Important invariant:
- `mbuild-text` always publishes payload as a **file** at `.mbuild/objects/<id>`.

## Recipe Shape

- `type = "text"`
- `artifact_kind = "..."`
- optional `outputs = [ ... ]`
- exactly one of:
  - `source = "<text-content>"` (single source)
  - `sources = { "output-name" = "<text-content>", ... }`

## Build Result

For each output:
- object payload file: `.mbuild/objects/<id>`
- metadata: `.mbuild/meta/<id>.ncl`
- ref symlink: `.mbuild/refs/<name> -> ../objects/<id>`

Current `id` is equal to output name.
