# Term-Centric Build Model

## Summary

`mbuild` embeds Nickel in Rust and interprets a STORE action language.
Nickel code builds STORE action trees. Rust evaluates the entry recipe to the
first action and then interprets the action tree step by step.

A realized `Build` value is the canonical build record stored in
`.mbuild/builds/<build_key>.json`.

## Layers

### 1. Nickel Layer

Nickel defines:

- pure helper values
- package sets and helper functions
- overrides
- STORE programs built from `return`, `bind`, and primitive builder actions

The top-level entry file must evaluate to one STORE action. `mbuild` does not
select a package field on its own. Any package-set selection is a frontend
concern inside the Nickel program.

Nickel may inspect builder-generated metadata through previously computed
`Build` values.

### 2. STORE Interpreter Layer

Rust loads the entry file, evaluates it to weak head normal form, and then
interprets the resulting STORE action tree.

At this layer, Rust understands the following action shapes conceptually:

- `Return value`
- `Bind { action, cont }`
- primitive builder actions such as `Text`, `Fetch`, `ContainerImage`,
  `Binary`, and `Image`

Rust builder implementations do not evaluate Nickel code directly. They are
invoked only by the STORE interpreter after the interpreter has decoded one
primitive builder action and its already-realized `Build` inputs.

### 3. Store Layer

The store persists:

- realized objects in `.mbuild/objects`
- realized build records in `.mbuild/builds`
- human-facing publication refs in `.mbuild/meta-refs` and `.mbuild/object-refs`

## STORE Programs

User-facing builder helpers have conceptual types like:

- `text : String -> TextPayload -> STORE Build`
- `fetch : String -> FetchPayload -> STORE Build`
- `container_image : String -> ContainerImagePayload -> STORE Build`
- `binary : String -> BinaryPayload -> Build -> Build -> Array Build -> STORE Build`
- `image : String -> ImagePayload -> Optional Build -> Array Build -> STORE Build`

The first argument is the publication name. It is consumed by the interpreter,
not by the Rust builder implementation.

Primitive builder actions do not recursively run dependency actions on their
own. If a dependency itself must be built, the Nickel program sequences that
work explicitly with `bind`.

## `Build`

`Build` is the canonical realized result of one builder invocation.

Its contents are exactly the contents of the corresponding build record stored
under `.mbuild/builds/<build_key>.json`.

A `Build` value contains at least:

- `build_key`
- `object_hash`
- `kind`
- `attrs`

It may also expose:

- `producer`
- `input_build_keys`

`Build` does not contain runtime-only fields such as local object paths.

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

Downstream builder actions consume `Build` values as inputs.

Nickel may inspect builder-generated metadata such as:

- `dep.kind`
- `dep.attrs.image_ref`
- `dep.attrs.image_digest`

However, Rust-builder semantics are defined only by:

- builder tag
- builder payload/config
- payload content of already-realized dependency objects

Dependency metadata is observational only. If downstream behavior should depend
on metadata such as `dep.kind` or `dep.attrs.*`, Nickel must copy that data
explicitly into the downstream builder payload. A Rust builder whose behavior
changes solely because dependency metadata differs is considered a model bug.

## Publication

Publication is implicit in STORE semantics.

Every primitive builder action carries a publication name. After the
interpreter computes or reuses the corresponding `Build` value, it updates:

- `meta-refs/<name>.json -> ../builds/<build_key>.json`
- `object-refs/<name> -> ../objects/<object_hash>`

There is no separate user-facing `Publish` operation in the language surface.
Publication is part of evaluating a named primitive builder action.

## Interpreter Algorithm

### Entry Evaluation

For one recipe entry file, `mbuild`:

1. loads the Nickel file
2. evaluates it to weak head normal form
3. expects a top-level STORE action
4. interprets the resulting action tree

Relative Nickel imports work normally through the standard Nickel import rules.
No package-selection mechanism is part of `mbuild` itself.

### `Bind` Evaluation

When the interpreter sees:

```nickel
'Bind { action = a, cont = k }
```

it:

1. interprets `a`
2. obtains a Nickel value `x`
3. applies `k x` inside Nickel
4. evaluates the resulting term to the next STORE action
5. continues interpretation

This is how dependency recursion is expressed.

### Primitive Builder Action Evaluation

For one primitive builder action, the interpreter:

1. decodes the publication name and builder payload
2. decodes already-realized input `Build` values
3. validates input kinds and required attrs
4. computes ordered `input_build_keys`
5. computes `build_key`
6. reuses an existing build record on cache hit
7. executes the registered Rust builder on cache miss
8. stores the produced payload in `objects/`
9. writes one build record in `builds/`
10. updates current publication refs for the supplied name
11. rotates the previous current refs into timestamp-suffixed history refs if
    the name already pointed at a different build
12. returns the resulting `Build`

## Worked Example

Consider a recipe entry file like:

```nickel
store.bind (store.fetch "bash-src-5.3" { ... }) (fun bashSrc =>
store.bind (store.text "buildscript-bash-stage2" { ... }) (fun bashScript =>
store.bind (store.container_image "bootstrap-image" { ... }) (fun bootstrapImage =>
store.binary "bash-stage2" { optimize = "size" } bootstrapImage bashScript [bashSrc])))
```

Execution proceeds as follows:

1. Nickel evaluates the file to the first `Bind` node.
2. Rust interprets the left `fetch` action and returns a realized `Build` for
   `bashSrc`.
3. Rust applies the continuation to that `Build` value inside Nickel.
4. Nickel evaluates the continuation result to the next `Bind` node.
5. Rust interprets the `text` action and returns a realized `Build` for
   `bashScript`.
6. The process repeats for `container_image`.
7. Eventually Nickel produces a primitive `binary` action whose dependency
   fields already contain the three realized `Build` values.
8. Rust interprets that `binary` action, computes or reuses its build result,
   updates refs for `bash-stage2`, and returns the final `Build`.

The interpreter alternates between Nickel evaluation and Rust-side STORE
execution until it reaches `Return` or a final primitive builder result.
