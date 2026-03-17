# Term-Centric Build Model

## Summary

`mbuild` embeds Nickel in Rust and interprets a STORE build language.
Primitive builder operations are evaluated by Rust, produce `Built` values, and
perform store effects during evaluation.

A `Built` value is the realized result of one builder invocation. It is exactly
the corresponding build record stored in `.mbuild/builds/<build_key>.json`.

## Layers

### 1. Nickel Layer

Nickel defines package sets, helper functions, overrides, and builder calls.
Nickel code manipulates:

- builder configuration values
- `Built` values
- recursive package sets and helper combinators

At this layer, users compose build programs and may inspect builder-generated
metadata through previously computed `Built` values.

### 2. STORE Layer

Primitive builder operations are STORE actions.
Operationally, they have the form:

- `Text : String -> TextPayload -> STORE Built`
- `Fetch : String -> FetchPayload -> STORE Built`
- `ContainerImage : String -> ContainerImagePayload -> STORE Built`
- `Binary : String -> BinaryPayload -> Built -> Built -> Array Built -> STORE Built`
- `Image : String -> ImagePayload -> Optional Built -> Array Built -> STORE Built`

The first argument is the publication name. It is consumed by the interpreter,
not by the Rust builder implementation.

After evaluation, the resulting Nickel value is a pure `Built` record.

### 3. Store Layer

The store persists:

- realized objects in `.mbuild/objects`
- realized build records in `.mbuild/builds`
- human-facing publication refs in `.mbuild/meta-refs` and `.mbuild/object-refs`

## `Built`

`Built` is the canonical realized result of one builder invocation.

Its contents are exactly the contents of the corresponding build record stored
under `.mbuild/builds/<build_key>.json`.

A `Built` value contains at least:

- `build_key`
- `object_hash`
- `kind`
- `attrs`

It may also expose:

- `producer`
- `input_build_keys`

`Built` does not contain runtime-only fields such as local object paths.

## Build Keys

`build_key` is the identity of one builder node in the dependency graph.

It is computed from:

- builder tag
- normalized payload
- ordered `input_build_keys`

It does not depend on:

- publication name
- authored recipe metadata
- `object_hash`

This makes `build_key` a graph identity rather than a payload identity.

## Dependency Semantics

Downstream builder calls consume `Built` values as inputs.

This gives Nickel access to builder-generated metadata such as:

- `dep.kind`
- `dep.attrs.image_ref`
- `dep.attrs.image_digest`

If authored recipe metadata should influence the build, Nickel must explicitly
place the relevant data into builder payloads. Rust builders do not receive
recipe metadata directly.

## Publication

Publication is implicit in STORE semantics.

Every primitive builder call carries a publication name as its first argument.
After the interpreter computes or reuses the corresponding `Built` value, it
updates:

- `meta-refs/<name>.json -> ../builds/<build_key>.json`
- `object-refs/<name> -> ../objects/<object_hash>`

There is no separate user-facing `Publish` operation in the language surface.
Publication is part of evaluating a named STORE action.

## Interpreter Algorithm

For one primitive builder call, the interpreter:

1. evaluates dependency arguments to `Built`
2. validates builder-specific input kinds and required attrs
3. computes ordered `input_build_keys`
4. computes `build_key` from builder tag, normalized payload, and
   `input_build_keys`
5. reuses an existing build record on cache hit
6. executes the registered Rust builder on cache miss
7. computes `object_hash` from the produced payload on cache miss
8. writes the payload into `objects/`
9. writes one build record into `builds/`
10. updates publication refs for the supplied name
11. returns the resulting `Built`

## Extensible Builder Operations

The set of primitive builder operations is open.

Nickel uses primitive builder calls, and Rust maintains a registry from builder
kind to implementation.

## Named Nodes and Anonymous Subexpressions

A builder call always carries an explicit publication name.

Publication names are therefore authored at the language level instead of being
inferred from store paths or from package-set field positions.

Anonymous helper expressions may still exist in Nickel, but publication is tied
to primitive builder calls rather than to arbitrary syntax nodes.
