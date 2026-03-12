# Term-Centric Build Model for `mbuild`

## Summary

This document describes the intended high-level execution model for `mbuild`.

The core idea is term-centric:

- Nickel defines a pure program made of builder terms.
- Primitive operations of that program are builder operations.
- Rust receives one selected closed artifact term and interprets it.
- Store layout, hashing, and caching are implementation details of the interpreter.

This is intentionally different from the current name-addressed recipe model.

## Layers

There are three conceptually separate layers.

### 1. Nickel language layer

Nickel is used to define a pure build program.

That program is composed from:

- builder operations;
- typed builder configuration values;
- typed artifact dependencies;
- package sets and helper combinators.

At this layer, users should think in terms of composing terms, not in terms of
store paths, artifact hashes, or cache lookup.

### 2. Interpreter layer

Rust acts as an interpreter for the Nickel term.

The interpreter is responsible for:

- recursively evaluating dependencies embedded in the term structure;
- validating builder-specific inputs;
- computing hashes for builder invocations and resulting payloads;
- checking whether results already exist in the store;
- executing builders on cache miss;
- publishing resulting objects and artifacts into the CAS store.

This means that store identity, object hashing, artifact hashing, caching, and
reuse are runtime semantics of the interpreter, not part of the Nickel API.

### 3. Store layer

The CAS store is a persistence and caching mechanism for evaluated results.

The store is not the programming model exposed to the Nickel author.

Nickel authors describe _what to build_. The interpreter decides:

- how to hash it;
- how to cache it;
- where to store it;
- whether it needs to be rebuilt.

## Builder Terms

Builder operations are the primitive operations of the Nickel program.

The intended representation is a tagged enum value with a single record payload.

Example shape:

```nickel
'Binary {
  outputs = ["out", "dev"],
  optimize = "size",
  image = imageArtifact,
  script = builderScriptArtifact,
  sources = [source1Artifact, source2Artifact],
}
```

Important properties:

- builder inputs and configuration live together as typed fields in the payload record;
- the payload is one record, even if the conceptual operation has several arguments;
- builder operations are pure terms and do not execute anything by themselves.

## Artifacts in Nickel

At the Nickel layer, an artifact is not a store object yet.

It is better understood as a pure value that denotes a build result produced by
some builder term.

This document intentionally separates:

- Nickel artifact terms;
- realized artifacts stored in the CAS store.

The same word "artifact" may be used for both in discussion, but the distinction
must remain clear in implementation.

## Multi-Output Builders

Multi-output builders should not expose a raw opaque term plus an explicit
`selectOutput` operation as the primary user-facing API.

Instead, the preferred model is:

- a builder operation returns a bundle;
- the bundle exposes named output projections;
- each projection is itself an artifact term.

Example:

```nickel
let zstd = mBinary {
  outputs = ["out", "dev"],
  image = bootstrapImage,
  script = buildScript,
  sources = [zstdSrc],
} in
{
  zstd = zstd.out,
  zstd_dev = zstd.dev,
}
```

Here `mFetch`, `mText`, `mBinary`, and similar names should be understood as
convenient user-facing helper names for builder operations. Their concrete
pseudo-definition is sketched later in [`NICKEL_SKETCH.md`](./NICKEL_SKETCH.md).

This preserves the important semantics:

- there is one underlying builder term;
- that term may produce multiple outputs;
- downstream users consume explicit output projections.

This is preferred over assigning the same raw term to multiple package fields and
trying to infer the intended output from the field name.

## Package Sets

`pkgs` is expected to be a Nickel record containing artifact terms or bundle
projections.

Example shape:

```nickel
let rec pkgs = {
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

Important points:

- package fields are a convenience layer for composition;
- names in `pkgs` are not the runtime identity of artifacts;
- the dependency graph is induced by term structure, not by a global namespace lookup.

## Dependency Graph and Evaluation

The dependency graph exists structurally inside the term.

For example, a binary builder term that embeds:

- an image artifact term;
- a build-script artifact term;
- an array of source artifact terms;

already defines its dependency edges.

The interpreter therefore does not need to build the graph from package names.
It discovers the graph by recursively traversing the selected closed term.

Cycle detection is still a runtime responsibility of the interpreter.
The language model does not need to guarantee acyclicity statically.

## Entry Term Selection

The intended CLI model is:

- by default, `mbuild` reads `./.mbuild/recipe.ncl`;
- that file is expected to evaluate to one selected closed artifact term or bundle projection;
- the user may alternatively pass another Nickel file path on the command line.

This entrypoint selection happens before interpretation.

Rust still receives only one selected closed artifact term.

## Interpreter Algorithm

Given one selected closed artifact term, the interpreter works conceptually as follows:

1. Receive the selected Nickel term.
2. Inspect the top-level builder operation or output projection.
3. Recursively interpret all embedded dependency terms.
4. Obtain realized dependency artifacts for the current builder invocation.
5. Compute the stable identity of the current builder invocation from:
   - builder operation tag;
   - typed builder payload;
   - realized input artifacts;
   - selected output projection, if applicable.
6. Check the CAS store for an existing realized result.
7. On cache hit, return the realized artifact.
8. On cache miss, execute the corresponding registered builder.
9. Publish resulting objects and artifact metadata to the CAS store.
10. Return the realized artifact or output bundle to the caller.

This is a recursive interpreter model, not a global name-resolution model.

## Extensible Primitive Operations

The set of primitive builder operations should be extensible.

The intended direction is:

- Nickel terms use tagged builder operations;
- the concrete set of supported tags is determined by registered builders in `mbuild`;
- Rust maintains a registry from builder tag to interpreter implementation.

Conceptually:

- the language-level representation is open to extension;
- the runtime can only interpret the operations that are actually registered.

This gives an extensible architecture without requiring every possible builder to
be hard-coded into one permanently closed global enum design.

## Typed Builder Inputs

The current model of untyped arrays of artifact names should evolve toward
builder-specific typed inputs.

Example intent:

- `Binary` takes:
  - builder-specific configuration fields in its payload record,
  - one image artifact,
  - one build-script artifact,
  - an array of source artifacts;
- `Fetch` takes:
  - only builder-specific scalar/record fields in its payload record;
- `Image` takes:
  - builder-specific configuration fields in its payload record,
  - a base image artifact or image bundle input,
  - an array of binary-output artifacts.

This should be expressed at the Nickel API level by the structure of each builder
payload record and the contracts or types attached to it.

## What Is Out of Scope Here

This document does not define:

- the exact CAS object or artifact JSON schema;
- the exact hash canonicalization rules;
- the exact on-disk store layout;
- the final set of builder operation tags;
- the exact encoding of open builder operation rows in Nickel;
- the exact shape of the Rust/Nickel boundary.

Those belong to lower-level design documents.

## Design Direction

The intended direction from this point is:

1. define the user-facing Nickel model for builder terms and bundles;
2. define the minimal artifact term abstraction exposed in Nickel;
3. define how one closed artifact term is passed to Rust;
4. implement a Rust interpreter for recursive term evaluation;
5. attach CAS storage and caching semantics behind that interpreter.

This keeps the user model simple and pushes operational complexity into the runtime,
where it belongs.
