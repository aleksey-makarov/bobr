# Nickel API Model

## Summary

The Nickel API exposes a STORE action language and realized `Build` values.
Rust embeds Nickel, evaluates the entry recipe to the first STORE action, and
interprets the resulting action tree.

The Nickel layer does not expose:

- raw store paths
- manual cache management
- explicit publication operations
- low-level CAS internals

## Entry Contract

`mbuild` executes one Nickel entry file.

The top-level result of that file must be a STORE action.

After Rust interprets that STORE program, `mbuild build` pretty-prints the
final Nickel value. The final result may therefore be any `STORE a`, not only
`STORE Build`.

`mbuild` does not select a package field or interpret package-set structure on
its own. If a recipe uses a recursive `pkgs` record, the recipe itself must
select which STORE program to return.

## Core STORE Combinators

Conceptual API:

- `return : a -> STORE a`
- `bind : STORE a -> (a -> STORE b) -> STORE b`
- `map : (a -> b) -> STORE a -> STORE b`
- `sequence : Array (STORE a) -> STORE (Array a)`
- `sequence_ : Array (STORE a) -> STORE null`
- `for_each : Array a -> (a -> STORE b) -> STORE null`

`bind` is the mechanism that sequences build dependencies. Primitive builder
helpers consume already-realized `Build` values, not unresolved STORE actions.

## Primitive Builder Helpers

Canonical forms:

- `text : String -> TextPayload -> STORE Build`
- `fetch : String -> FetchPayload -> STORE Build`
- `container_image : String -> ContainerImagePayload -> STORE Build`
- `binary : String -> BinaryPayload -> Build -> Build -> Array Build -> STORE Build`
- `image : String -> ImagePayload -> Optional Build -> Array Build -> STORE Build`

The first argument is the publication name:

- it is used by the interpreter for implicit publication
- it does not participate in `build_key`
- Rust builders do not receive it in their builder-specific config

## Builder Payloads

Builder-specific configuration is carried by ordinary payload records.

Examples:

```nickel
store.text "buildscript-bash-stage2" {
  kind = "build-script",
  source = "#!/usr/bin/env bash\n...",
}
```

```nickel
store.fetch "bash-src-5.3" {
  url = ["https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz"],
  hash = "sha256:...",
}
```

A primitive builder helper that depends on previously built values is normally
used under `bind`:

```nickel
store.bind (store.fetch "bash-src-5.3" { ... }) (fun bashSrc =>
store.bind (store.text "buildscript-bash-stage2" { ... }) (fun bashScript =>
store.bind (store.container_image "bootstrap-image" { ... }) (fun bootstrapImage =>
store.binary "bash-stage2" { optimize = "size" } bootstrapImage bashScript [bashSrc])))
```

Builder payloads do not contain publication names.

`binary` also accepts an optional `script_config` field. When present, `mbuild`
materializes it as a read-only config directory inside the container and sets
`MBUILD_SCRIPT_CONFIG_DIR=/__mbuild_script_config`.

For `binary`, the `sources` array is ordered:

- the `sources` array may be empty for source-free filesystem artifact builders
- if present, the first source is the primary source tree and becomes `MBUILD_SOURCE_INPUT`
- additional source inputs may be `source-tree`, `fetched-file`, or `binary-output`
- auxiliary directories are mounted as `/in/sourcesN`
- auxiliary fetched files are mounted as `/in/sourcesN`
- if `script_config` is present, records and arrays are lowered into a
  directory tree under `/__mbuild_script_config`
- string leaves become regular files with their exact contents

## `Build` Values

A `Build` value is the realized result of one builder invocation.

It is the public build handle stored under `.mbuild/builds/<build_key>`, which
resolves to a canonical result record under
`.mbuild/results/<result_key>.json`.

At minimum, a `Build` value exposes:

- `build_key`
- `created_at`
- `object_hash`
- `kind`
- `attrs`

This lets Nickel code inspect builder-generated metadata from dependency values.
This metadata is observational. If downstream build behavior should depend on
it, Nickel must copy the relevant data explicitly into the downstream builder
payload. Builders are defined to depend only on their payload/config and on the
payload content of realized input objects.

Examples:

- `bootstrapImage.kind`
- `bootstrapImage.attrs.image`
- `bootstrapImage.attrs.image_id`

## Implicit Publication

Publication is implicit in primitive builder evaluation.

Evaluating a primitive builder action with publication name `name` updates:

- `.mbuild/meta-refs/<name>.json`
- `.mbuild/object-refs/<name>`

If the name already points at a different current build, the previous current
refs are rotated into timestamp-suffixed history refs:

- `.mbuild/meta-refs/<name>.<YYMMDDHHMMSS>.json`
- `.mbuild/object-refs/<name>.<YYMMDDHHMMSS>`

The returned value is still the same `Build` record.

## Relationship to the Store

The interpreter translates primitive builder evaluation into:

- realized objects in `.mbuild/objects`
- public build-handle refs in `.mbuild/builds`
- canonical result records in `.mbuild/results`
- human-facing metadata refs in `.mbuild/meta-refs`
- human-facing object refs in `.mbuild/object-refs`

Authored recipe metadata does not belong to the store execution model unless it
is explicitly placed into builder payloads.
