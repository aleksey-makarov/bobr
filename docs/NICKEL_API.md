# Nickel API Model for `mbuild`

## Summary

This document defines the intended user-facing Nickel model for `mbuild`.

It connects:

- the term-centric execution model from [`TERM_MODEL.md`](./TERM_MODEL.md);
- the object store and publication model from [`CAS.md`](./CAS.md).

The Nickel layer should expose a pure API for composing builder terms. Users write
Nickel programs in terms of objects, builder operations, and output bundles. The
Nickel API must not expose store paths, object hashes, cache management details,
or low-level publication mechanics.

## Core Model

### Object

At the Nickel layer, an object is a pure value denoting a build result.

It is not:

- a store path;
- a realized payload in `.mbuild/objects`;
- a metadata record on disk.

Instead, it is the user-facing handle for a build result inside the Nickel term graph.

### Builder operation

A builder operation is a tagged enum variant with a single record payload.

Conceptual examples:

```nickel
'Fetch {
  url = [...],
  hash = "sha256:...",
}

'Binary {
  outputs = ["out", "dev"],
  optimize = "size",
  image = imageObject,
  script = builderScriptObject,
  sources = [source1Object, source2Object],
}
```

Rules:

- each builder operation has exactly one record payload;
- builder-specific inputs are represented by typed fields in that payload;
- builder-specific configuration also lives in ordinary payload fields;
- untyped arrays of object names are replaced with typed object arguments;
- builder terms are pure and do not execute by themselves;
- builder payloads must not contain publication metadata such as names or aliases.

## Top-Level Build Request

The runtime entrypoint is one evaluated request, not a bare builder term.

Minimal conceptual shape:

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

- `build` is the pure build term interpreted by Rust;
- `meta` is metadata used to publish refs and human-facing descriptions;
- `meta` is not part of builder semantics;
- `meta` does not affect object identity.

This lets the runtime create publication records without polluting builder
terms with non-semantic metadata.

## Builder Results

### Single-output builders

A single-output builder returns one object.

Conceptually:

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

Rules:

- one builder term may expose multiple output projections;
- each projection is itself an object term;
- users consume projections like ordinary record fields;
- field names on the bundle are the builder-declared output labels.

## Package Sets

`pkgs` is a Nickel record that contains:

- object terms;
- bundle records returned by multi-output builders;
- helper values and configuration records.

Important properties:

- names in `pkgs` are for composition and convenience;
- names are not object identity;
- names are not part of store semantics;
- dependency structure comes from embedded object terms, not from global string lookup.

## Extensible Builder Operations

The set of primitive builder operations is intended to be extensible.

The design direction is:

- the Nickel layer uses tagged builder operations;
- the concrete supported builder tags are determined by the registered builders in `mbuild`;
- Rust maintains a registry from builder tag to interpreter implementation.

## Typed Builder Inputs

Builder payloads should carry typed object inputs.

Example intent:

- `Binary` takes:
  - builder-specific payload fields;
  - one image object;
  - one build-script object;
  - an array of source objects;
- `Fetch` takes:
  - builder-specific payload fields and no object dependencies;
- `Image` takes:
  - builder-specific payload fields;
  - zero or one base image object, depending on mode;
  - an array of binary-output objects.

Output typing may remain weaker than input typing in early versions. That is acceptable.

## Selected Request

The Rust interpreter should receive one selected build request.

This means:

- the default top-level entrypoint is `./.mbuild/recipe.ncl`;
- that file is expected to evaluate to one request with fields `meta` and `build`;
- a caller may alternatively provide another Nickel file path;
- the final selected `build` term is closed before interpretation;
- Rust interprets that `build` term recursively and uses `meta` only for publication.

The interpreter should not depend on a persistent global namespace of names in the store.

## Relationship to CAS

The Nickel API does not expose:

- object hashes;
- store paths;
- cache lookup;
- publication files;
- ref layouts.

Those belong to the interpreter and store layers.

The interpreter is responsible for translating Nickel object terms into:

- realized objects in `.mbuild/objects`;
- publication records in `.mbuild/meta-refs`;
- human-facing object refs in `.mbuild/object-refs`.

## Minimal API Direction

The intended user-facing API direction is:

- constructor-like functions or tagged values for each registered builder;
- builder-specific record payloads;
- bundles for multi-output builders;
- package sets composed from object terms;
- one selected request passed into Rust.

This is the minimum model needed before specifying lower-level encoding or Rust/Nickel
FFI details.

## Out of Scope

This document does not define:

- the exact Nickel syntax for open builder operation rows;
- the exact `Object` type encoding in Nickel;
- the exact contracts for every builder payload;
- the Rust-side representation of evaluated Nickel values;
- the exact bundle typing strategy for multi-output builders;
- the exact CLI spelling used to choose the selected request.
