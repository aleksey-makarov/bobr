# Term-Centric Build Model for `mbuild`

## Summary

This document describes the intended high-level execution model for `mbuild`.

The core idea is term-centric:

- Nickel defines a pure program made of builder terms.
- Primitive operations of that program are builder operations.
- Rust receives one selected evaluated build request, extracts its `build` term, and interprets it.
- Store layout, hashing, and caching are implementation details of the interpreter.

This is intentionally different from the current name-addressed recipe model.

## Layers

There are four conceptually separate layers.

### 1. Nickel term layer

Nickel is used to define a pure build program.

That program is composed from:

- builder operations;
- typed builder configuration values;
- typed object dependencies;
- package sets and helper combinators.

At this layer, users should think in terms of composing terms, not in terms of
store paths, object hashes, or cache lookup.

### 2. Request layer

The runtime entrypoint is not a bare build term.

Nickel evaluates to one build request of the conceptual shape:

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

The key rule is:

- `meta` is publication metadata;
- `build` is the pure build term.

This keeps builder operations pure and reusable while still giving the runtime a
place to obtain publication metadata.

### 3. Interpreter layer

Rust acts as an interpreter for the Nickel build term.

The interpreter is responsible for:

- recursively evaluating dependencies embedded in the term structure;
- validating builder-specific inputs;
- computing object hashes for resulting payloads;
- checking whether objects already exist in the store;
- executing builders on cache miss;
- publishing resulting objects, object metadata, and publication metadata.

This means that store identity, object hashing, caching, and reuse are runtime
semantics of the interpreter, not part of the Nickel API.

The interpreter receives one selected request.

From that request it extracts:

- publication metadata in `meta`;
- the build term in `build`.

Builders consume only the build term and runtime context. They do not consume
publication metadata directly.

### 4. Store layer

The CAS store is a persistence and caching mechanism for evaluated results.

The store is not the programming model exposed to the Nickel author.

Nickel authors describe _what to build_. The interpreter decides:

- how to hash it;
- how to cache it;
- where to store it;
- how to attach technical metadata;
- how to publish it under human-facing names.

## Builder Terms

Builder operations are the primitive operations of the Nickel program.

The intended representation is a tagged enum value with a single record payload.

Example shape:

```nickel
'Binary {
  outputs = ["out", "dev"],
  optimize = "size",
  image = imageObject,
  script = builderScriptObject,
  sources = [source1Object, source2Object],
}
```

Important properties:

- builder inputs and configuration live together as typed fields in the payload record;
- the payload is one record, even if the conceptual operation has several arguments;
- builder operations are pure terms and do not execute anything by themselves;
- builder payloads do not contain publication metadata such as names.

## Objects in Nickel

At the Nickel layer, a built object is not a store object yet.

It is better understood as a pure value that denotes a build result produced by
some builder term.

This document intentionally separates:

- Nickel object terms;
- realized objects stored in the CAS store.

The same word "object" may be used for both in discussion, but the distinction
must remain clear in implementation.

## Multi-Output Builders

Multi-output builders should not expose a raw opaque term plus an explicit
`selectOutput` operation as the primary user-facing API.

Instead, the preferred model is:

- a builder operation returns a bundle;
- the bundle exposes named output projections;
- each projection is itself an object term.

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

`pkgs` is expected to be a Nickel record containing object terms or bundle
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
- names in `pkgs` are not object identity;
- the dependency graph is induced by term structure, not by a global namespace lookup.

## Dependency Graph and Evaluation

The dependency graph exists structurally inside the term.

For example, a binary builder term that embeds:

- an image object term;
- a build-script object term;
- an array of source object terms;

already defines its dependency edges.

The interpreter therefore does not need to build the graph from package names.
It discovers the graph by recursively traversing the selected closed `build` term.

Cycle detection is still a runtime responsibility of the interpreter.
The language model does not need to guarantee acyclicity statically.

## Entry Request Selection

The intended CLI model is:

- by default, `mbuild` reads `./.mbuild/recipe.ncl`;
- that file is expected to evaluate to one selected request with fields `meta` and `build`;
- the user may alternatively pass another Nickel file path on the command line.

This entrypoint selection happens before interpretation.

Rust still receives only one selected request.

## Interpreter Algorithm

Given one selected request, the interpreter works conceptually as follows:

1. Receive the selected Nickel request.
2. Extract the `meta` record and the `build` term.
3. Inspect the top-level builder operation or output projection.
4. Recursively interpret all embedded dependency terms.
5. Obtain realized dependency objects for the current builder invocation.
6. Compute the stable identity of the current result from the produced payload only.
7. Check the CAS store for an existing object with the same `object-hash`.
8. On cache hit, return the realized object.
9. On cache miss, execute the corresponding registered builder.
10. Publish:
    - the resulting object in `objects/`;
    - technical metadata in `meta/`;
    - publication metadata and refs from the request `meta`.
11. Return the realized object or output bundle to the caller.

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

The current model of untyped arrays of object names should evolve toward
builder-specific typed inputs.

Example intent:

- `Binary` takes:
  - builder-specific configuration fields in its payload record,
  - one image object,
  - one build-script object,
  - an array of source objects;
- `Fetch` takes:
  - only builder-specific scalar/record fields in its payload record;
- `Image` takes:
  - builder-specific configuration fields in its payload record,
  - a base image object or image bundle input,
  - an array of binary-output objects.

This should be expressed at the Nickel API level by the structure of each builder
payload record and the contracts or types attached to it.
