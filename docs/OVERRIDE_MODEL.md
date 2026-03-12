# Override Model for `mbuild`

## Summary

This document records the intended direction for a Nix-like override mechanism in
`mbuild`.

The key design choice is:

- override lives in the Nickel layer;
- Rust does not implement override semantics directly;
- Rust receives the final selected closed artifact term after all overrides have
  already been applied.

This keeps override as a pure source-level transformation and keeps CAS, hashing,
and caching entirely in the interpreter/runtime layer.

## Position in the Architecture

This document is consistent with:

- [`TERM_MODEL.md`](./TERM_MODEL.md): terms are pure Nickel programs interpreted by Rust;
- [`NICKEL_API.md`](./NICKEL_API.md): users compose builder terms and bundles in Nickel;
- [`NICKEL_SKETCH.md`](./NICKEL_SKETCH.md): package sets expose artifacts and bundle projections;
- [`CAS.md`](./CAS.md): object and artifact identity are runtime concerns, not user-facing API.

Override should therefore be understood as a Nickel-level operation that produces a
new term or package value before interpretation.

## Core Idea

An override is a pure transformation of package definitions or builder arguments.

Conceptually:

- users define package values in Nickel;
- those values are parameterized by arguments and dependencies;
- `override` creates a new package value with modified arguments;
- the resulting package yields a different final artifact term;
- Rust interprets only that final artifact term.

Rust does not need to know whether a term came from:

- a base package definition;
- one override;
- several nested overrides.

It only sees the final term and computes identity from that term and its dependencies.

## What Override Operates On

Override should operate on Nickel package abstractions, not directly on the CAS store.

In particular, override should not manipulate:

- object hashes;
- artifact hashes;
- store refs;
- object records;
- artifact records.

Instead, override should work on:

- builder configuration records;
- package argument records;
- package dependency bindings;
- bundle-producing package constructors.

## Recommended Package Shape

To support ergonomic override, a package should conceptually be more than a bare
artifact term.

The intended direction is that a package definition is parameterized and can be
reinstantiated with changed arguments.

Conceptually:

```nickel
mkPackage = fun defaults =>
  {
    args = defaults,
    outputs = buildWith defaults,
    override = fun patch =>
      mkPackage (defaults & patch),
  }
```

This is not final syntax, but it captures the intended semantics:

- a package has some default arguments;
- outputs are derived from those arguments;
- `override` merges a patch into the arguments and rebuilds the package value.

## Kinds of Override

### 1. Replace a package binding

The simplest form:

```nickel
pkgs // { zstd = myZstd }
```

This is ordinary record-level replacement.

### 2. Override builder configuration

Example intent:

```nickel
pkgs.zstd.override {
  optimize = "size",
}
```

This should produce a new package value whose generated term differs only in the
selected config fields.

### 3. Override dependencies

Example intent:

```nickel
pkgs.zstd.override {
  image = pkgs.altBootstrapImage,
  script = pkgs.altBuildScript,
}
```

This should produce a new package value whose generated term points to different
artifact dependencies.

## Interaction with Multi-Output Bundles

Override should apply to the package or builder term that produces the bundle,
not to already projected outputs.

Conceptually:

```nickel
let zstdPkg = mkZstdPackage { ... } in
let zstdAlt = zstdPkg.override { image = pkgs.altImage } in
{
  zstd = zstdAlt.outputs.out,
  zstd_dev = zstdAlt.outputs.dev,
}
```

This preserves the intended model:

- one underlying builder term;
- one override applied at the package-definition level;
- multiple output projections derived afterward.

## Interaction with the Interpreter

The interpreter does not provide an override primitive.

Its job begins only after the final term is selected.

Operationally:

1. Nickel evaluates package definitions and overrides.
2. Nickel produces a final selected closed artifact term.
3. Rust interprets that term recursively.
4. Rust computes hashes and performs CAS/cache lookup.
5. Rust executes builders only when needed.

Thus override changes the final term, and that in turn changes the interpreter-visible identity.

## Relationship to Hashing and Caching

Override affects runtime behavior only indirectly:

- different override inputs produce a different final term;
- a different final term produces a different runtime identity;
- different identity means different cache/store entries unless the resulting terms
  normalize to the same effective build.

This is exactly the intended behavior.

No special “override support” is needed in CAS itself.

## Why This Direction Is Valuable

This keeps concerns separated:

- Nickel: package abstraction, composition, override, builder arguments;
- Rust interpreter: recursion, hashing, caching, execution;
- CAS store: persistence of realized results.

It also preserves a Nix-like user experience without forcing Nix-like store
semantics into the user-facing language layer.

## Out of Scope

This document does not define:

- the exact user-facing syntax of `override`;
- whether there will be distinct helpers such as `override`, `overrideAttrs`, or `overrideInputs`;
- the final package wrapper representation in Nickel;
- the exact merge semantics for override patches;
- how override interacts with future module-like or overlay-like mechanisms.

These should be decided only after the base term and package model is considered stable.
