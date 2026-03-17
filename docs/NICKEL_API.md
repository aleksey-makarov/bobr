# Nickel API Model

## Summary

The Nickel API exposes primitive builder operations and realized `Built` values.
Rust embeds Nickel and interprets these builder operations with STORE semantics.

The Nickel layer does not expose:

- raw store paths
- manual cache management
- explicit publication operations
- low-level CAS internals

## Primitive Builder Operations

Primitive builder operations are named STORE actions.

Canonical forms:

- `mText : String -> TextPayload -> Built`
- `mFetch : String -> FetchPayload -> Built`
- `mContainerImage : String -> ContainerImagePayload -> Built`
- `mBinary : String -> BinaryPayload -> Built -> Built -> Array Built -> Built`
- `mImage : String -> ImagePayload -> Optional Built -> Array Built -> Built`

Operationally these are STORE actions interpreted by Rust. After evaluation,
the resulting Nickel value is a pure `Built` value.

The first argument is the publication name:

- it is used by the interpreter for implicit publication
- it does not participate in `build_key`
- Rust builders do not receive it in their builder-specific config

## Builder Payloads

Builder-specific configuration is carried by ordinary payload records.

Examples:

```nickel
mText "buildscript-bash-stage2" {
  kind = "build-script",
  source = "#!/usr/bin/env bash\n...",
}
```

```nickel
mFetch "bash-src-5.3" {
  url = ["https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz"],
  hash = "sha256:...",
}
```

```nickel
mBinary "bash-stage2" {
  optimize = "size",
} bootstrapImage bashScript [bashSrc]
```

Builder payloads do not contain publication names.

## `Built` Values

A `Built` value is the realized result of one builder invocation.

It is exactly the corresponding build record stored in
`.mbuild/builds/<build_key>.json`.

At minimum, a `Built` value exposes:

- `build_key`
- `object_hash`
- `kind`
- `attrs`

This lets Nickel code inspect builder-generated metadata from dependency values.

Examples:

- `pkgs.bootstrapImage.kind`
- `pkgs.bootstrapImage.attrs.image_ref`
- `pkgs.bootstrapImage.attrs.image_digest`

## Package Sets

`pkgs` is a recursive Nickel record used for package composition.

Fields in `pkgs` may refer to:

- builder calls
- helper values
- configuration records
- previously computed `Built` values

Package-set field names are a composition mechanism. They are not store
identity and they do not replace explicit publication names authored for
builder calls.

## Implicit Publication

Publication is implicit in primitive builder evaluation.

Evaluating a primitive builder call with publication name `name` updates:

- `.mbuild/meta-refs/<name>.json`
- `.mbuild/object-refs/<name>`

The returned value is still the same `Built` record.

## Relationship to the Store

The interpreter translates primitive builder evaluation into:

- realized objects in `.mbuild/objects`
- realized build records in `.mbuild/builds`
- human-facing metadata refs in `.mbuild/meta-refs`
- human-facing object refs in `.mbuild/object-refs`

Authored recipe metadata does not belong to the store execution model unless it
is explicitly placed into builder payloads.
