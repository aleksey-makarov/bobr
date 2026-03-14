# Term-Centric Build Model

## Summary

`mbuild` realizes build requests as content-addressed objects.

Nickel may define the build program, but `mbuild` itself consumes an already
materialized build request. Store layout, hashing, build recording, and caching
are interpreter concerns.

## Layers

### 1. Term Layer

Nickel defines a pure build program composed from:

- builder operations
- typed builder configuration values
- typed object dependencies
- package sets and helper combinators

At this layer, users compose terms. They do not refer to store paths, object
hashes, build keys, or cache lookup.

### 2. Request Layer

A build request is the runtime entrypoint, not a bare build term.

A build request has the shape:

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

`meta` is publication metadata.

`build` is the pure build term.

### 3. Interpreter Layer

Rust interprets the `build` term recursively.

The interpreter:

- evaluates dependency terms
- validates builder-specific inputs
- computes `build_key` from builder tag, normalized payload, resolved input object hashes, and selected output projection when projection exists
- reuses existing build records on matching `build_key`
- executes builders on cache miss
- computes object hashes from produced payloads on cache miss
- publishes resulting objects and build refs

### 4. Store Layer

The store persists realized objects, build records, and publication refs.

Nickel authors describe what to build. The interpreter decides:

- how to hash results
- how to key build records
- where to store results
- how to publish them under names

## Builder Terms

Builder operations are tagged enum values with one record payload.

Example:

```nickel
'Binary {
  outputs = ["out", "dev"],
  optimize = "size",
  image = imageObject,
  script = builderScriptObject,
  sources = [source1Object, source2Object],
}
```

Properties:

- builder inputs and configuration live together in the payload record
- the payload is one record
- builder operations are pure terms and do not execute anything by themselves
- builder payloads do not contain publication metadata such as names

## Objects in Nickel

At the Nickel layer, an object is a pure value denoting a build result.

It is distinct from:

- a realized store object in `.mbuild/objects`
- a build record in `.mbuild/builds`

## Multi-Output Builders

A multi-output builder returns a bundle.

The bundle exposes named output projections, and each projection is itself an
object term.

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

## Package Sets

`pkgs` is a Nickel record containing object terms or bundle projections.

Package field names are a composition convenience. They are not object identity
and they are not build-record identity.

## Dependency Graph

The dependency graph exists structurally inside the term.

The interpreter discovers the graph by recursively traversing the selected
closed `build` term. It does not resolve dependencies from a global namespace of
names.

Cycle detection is a runtime responsibility of the interpreter.

## Request Selection

`mbuild` reads one serialized build request either from
`./.mbuild/request.json` by default or from an explicitly selected
build-request JSON file.

## Interpreter Algorithm

Given one selected request, the interpreter:

1. receives the request
2. extracts `meta` and `build`
3. inspects the top-level builder operation or output projection
4. recursively interprets embedded dependency terms
5. obtains realized dependency objects through their build records
6. computes a `build_key` for the current interpreted builder invocation from builder tag, normalized payload, resolved input object hashes, and selected output projection when projection exists
7. reuses an existing build record on matching `build_key`
8. executes the registered builder on cache miss
9. computes the stable identity of the current result from the produced payload only on cache miss
10. publishes:
   - the resulting object in `objects/`
   - one build record in `builds/`
   - one metadata ref in `meta-refs/`
   - one object ref in `object-refs/`
11. returns the realized object or output bundle

## Extensible Builder Operations

The set of builder operations is open.

Nickel terms use tagged builder operations, and Rust maintains a registry from
builder tag to interpreter implementation.

## Typed Builder Inputs

Builder payloads carry typed object inputs.

Examples:

- `Binary` takes builder-specific payload fields, one image object, one build-script object, and an array of source objects
- `Fetch` takes builder-specific payload fields and no object dependencies
- `Image` takes builder-specific payload fields, zero or one base image object, and an array of binary-output objects
