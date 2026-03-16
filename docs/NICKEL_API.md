# Nickel API Model

## Summary

The Nickel API exposes pure builder terms, object bundles, package sets, and one
top-level build request.

It does not expose store paths, object hashes, build keys, cache management, or
low-level publication mechanics.

## Objects

At the Nickel layer, an object is a pure value denoting a build result.

It is not:

- a store path
- a realized payload in `.mbuild/objects`
- a build record in `.mbuild/builds`

## Builder Operations

A builder operation is a tagged enum variant with a single record payload.

Examples:

```nickel
'Binary {
  outputs = ["out", "dev"],
  optimize = "size",
  image = imageObject,
  script = builderScriptObject,
  sources = [source1Object, source2Object],
}
```

Rules:

- each builder operation has exactly one record payload
- builder-specific inputs are represented by typed fields in that payload
- builder-specific configuration also lives in ordinary payload fields
- builder terms are pure and do not execute by themselves
- builder payloads do not contain publication metadata

## Top-Level Build Request

The runtime entrypoint is one build request.

Shape:

```nickel
{
  meta = {
    name = "buildscript-coreutils",
    description = "...",
    aliases = [],
  },
  build = 'Text {
    kind = "build-script",
    source = "...",
  },
}
```

Semantics:

- `build` is the pure build term interpreted by Rust
- `meta` is metadata used to publish refs and human-facing descriptions
- `meta` does not affect object identity
- `meta` does not affect build-record identity
- `mbuild` consumes this request as plain data and does not evaluate Nickel itself

## Builder Results

### Single-output builders

A single-output builder returns one object.

Examples:

- `mFetch : FetchPayload -> Object SourceTree`
- `mText : TextPayload -> Object BuildScript`

### Multi-output builders

Multi-output builders return a bundle record whose fields are object terms.

Example:

```nickel
let zstd = mBinary {
  outputs = ["out", "dev"],
  image = bootstrapImage,
  script = buildScript,
  sources = [zstdSrc],
} in
{
  runtime = zstd.out,
  dev = zstd.dev,
}
```

## Package Sets

`pkgs` is a Nickel record containing:

- object terms
- bundle records returned by multi-output builders
- helper values and configuration records

Names in `pkgs` are composition conveniences. They are not object identity and
not build-record identity.

## Extensible Builder Operations

The set of primitive builder operations is defined by the builders registered in
`mbuild`.

Rust maintains a registry from builder tag to interpreter implementation.

## Typed Builder Inputs

Builder payloads carry typed object inputs.

Examples:

- `Binary` takes payload fields, one container-image object, one build-script
  object, and an array of source-tree objects
- `Fetch` takes payload fields and no object dependencies
- `Image` takes payload fields, zero or one base image object, and an array of
  binary-output objects

## Selected Request

The Rust interpreter receives one selected build request.

`mbuild` reads one serialized build request either from
`./.mbuild/request.json` by default or from an explicitly selected build-request
JSON file.

The selected request contains `meta` and `build`. Rust interprets `build`
recursively and uses `meta` only for publication.

## Relationship to the Store

The interpreter translates Nickel object terms into:

- realized objects in `.mbuild/objects`
- build records in `.mbuild/builds`
- human-facing metadata refs in `.mbuild/meta-refs`
- human-facing object refs in `.mbuild/object-refs`
