# Nickel API Model for `mbuild`

## Summary

This document defines the intended user-facing Nickel model for `mbuild`.

It connects:

- the term-centric execution model from [`TERM_MODEL.md`](./TERM_MODEL.md);
- the CAS and identity model from [`CAS.md`](./CAS.md).

The Nickel layer should expose a pure API for composing builder terms. Users write
Nickel programs in terms of artifacts, builder operations, and output bundles. The
Nickel API must not expose store paths, object hashes, artifact hashes, or cache
management details.

## Core Model

### Artifact

At the Nickel layer, an `Artifact` is a pure value denoting a build result.

It is not:

- a store path;
- a realized object;
- a CAS record on disk.

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
  image = imageArtifact,
  script = builderScriptArtifact,
  sources = [source1Artifact, source2Artifact],
}
```

Rules:

- each builder operation has exactly one record payload;
- builder-specific inputs are represented by typed fields in that payload;
- builder-specific configuration also lives in ordinary payload fields;
- untyped arrays of artifact names are replaced with typed artifact arguments;
- builder terms are pure and do not execute by themselves.

## Builder Results

### Single-output builders

A single-output builder returns an `Artifact`.

Conceptually:

- `mFetch : FetchPayload -> Artifact SourceTree`
- `mText : TextPayload -> Artifact BuildScript`

The exact Nickel type syntax may differ, but the semantic model should be this direct.

### Multi-output builders

Multi-output builders return a bundle record whose fields are artifact terms.

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
- each projection is itself an artifact term;
- users consume projections like ordinary record fields;
- field names on the bundle are the builder-declared output labels.

This is the preferred user-facing API over an explicit `selectOutput(term, "out")`
operation.

## Package Sets

`pkgs` is a Nickel record that contains:

- artifact terms;
- bundle records returned by multi-output builders;
- helper values and configuration records.

Example:

```nickel
let rec pkgs = {
  bootstrapImage = mContainerImage { ... },
  zstdSrc = mFetch { ... },
  zstdScript = mText { ... },

  zstdTerm = mBinary {
    outputs = ["out", "dev"],
    image = pkgs.bootstrapImage,
    script = pkgs.zstdScript,
    sources = [pkgs.zstdSrc],
  },

  zstd = pkgs.zstdTerm.out,
  zstd_dev = pkgs.zstdTerm.dev,
} in
pkgs
```

Important properties:

- names in `pkgs` are for composition and convenience;
- names are not artifact identity;
- names are not part of CAS semantics;
- dependency structure comes from embedded artifact terms, not from global string lookup.

## Extensible Builder Operations

The set of primitive builder operations is intended to be extensible.

The design direction is:

- the Nickel layer uses tagged builder operations;
- the concrete supported builder tags are determined by the registered builders in `mbuild`;
- Rust maintains a registry from builder tag to interpreter implementation.

This means the user-facing language model should not assume one permanently closed
global set of builder operations.

## Typed Builder Inputs

Builder payloads should carry typed artifact inputs.

Example intent:

- `Binary` takes:
  - builder-specific payload fields;
  - one image artifact;
  - one build-script artifact;
  - an array of source artifacts;
- `Fetch` takes:
  - builder-specific payload fields and no artifact dependencies;
- `Image` takes:
  - builder-specific payload fields;
  - zero or one base image artifact, depending on mode;
  - an array of binary-output artifacts.

Output typing may remain weaker than input typing in early versions. That is acceptable.

## Selected Closed Artifact Term

The Rust interpreter should receive one selected closed artifact term.

This means:

- the default top-level entrypoint is `./.mbuild/recipe.ncl`;
- that file may either define one term directly or select one term from a larger package set;
- a caller may alternatively provide another Nickel file path;
- the final selected term is closed before interpretation;
- Rust interprets that one selected term recursively.

The interpreter should not depend on a persistent global namespace of names in the store.

## Relationship to CAS

The Nickel API does not expose:

- object hashes;
- artifact hashes;
- store paths;
- cache lookup;
- store refs.

Those belong to the interpreter and CAS layers.

The interpreter is responsible for translating Nickel artifact terms into:

- realized objects in `.mbuild/objects`;
- realized artifact records in `.mbuild/artifacts`;
- optional human-facing refs.

## Minimal API Direction

The intended user-facing API direction is:

- constructor-like functions or tagged values for each registered builder;
- builder-specific record payloads;
- bundles for multi-output builders;
- package sets composed from artifact terms;
- one selected closed artifact term passed into Rust.

This is the minimum model needed before specifying lower-level encoding or Rust/Nickel
FFI details.

## Out of Scope

This document does not define:

- the exact Nickel syntax for open builder operation rows;
- the exact `Artifact` type encoding in Nickel;
- the exact contracts for every builder payload;
- the Rust-side representation of evaluated Nickel values;
- the exact bundle typing strategy for multi-output builders;
- the exact CLI spelling used to choose the selected term.
