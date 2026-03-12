# Pseudo-Nickel Sketch for `mbuild`

## Purpose

This document is a sketch of the intended user-facing Nickel API.

It is intentionally not a final syntax specification. The goal is to make the
term model concrete enough to reason about:

- builder operations;
- typed builder inputs;
- multi-output bundles;
- package-set composition;
- the idea of selecting one closed artifact term for interpretation.

## Core Intuition

Users write pure Nickel expressions that compose builder operations.

They should be able to think in this style:

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

Here:

- `mFetch`, `mText`, `mBinary` are builder operations;
- `pkgs.zstdSrc`, `pkgs.zstdScript`, `pkgs.zstd` are artifact terms;
- `pkgs.zstdTerm` is a multi-output bundle;
- `.out` and `.dev` are output projections.

## Artifact and Bundle Sketch

This is pseudo-Nickel, not final syntax:

```nickel
let Artifact = 'Artifact in
let Bundle = 'Bundle in
...
```

The exact encoding is undecided, but semantically:

- an `Artifact` is a pure term denoting one build result;
- a `Bundle` is a record whose fields are `Artifact`s;
- a single-output builder returns an `Artifact`;
- a multi-output builder returns a `Bundle`.

## Builder Operation Sketch

Builder operations are tagged values with one record payload.

### Fetch

```nickel
let mFetch = fun payload =>
  'Fetch {
    url = payload.url,
    hash = payload.hash,
  }
```

Intended meaning:

- no artifact inputs;
- one output artifact, usually a `source-tree` or `fetched-file`.

### Text

```nickel
let mText = fun payload =>
  'Text {
    artifact_kind = payload.artifact_kind,
    source = payload.source,
  }
```

Intended meaning:

- no artifact inputs;
- one output artifact, for example a `build-script`.

### Binary

```nickel
let mBinary = fun payload =>
  'Binary {
    outputs = payload.outputs,
    optimize = payload.optimize,
    image = payload.image,
    script = payload.script,
    sources = payload.sources,
  }
```

Intended meaning:

- builder-specific configuration fields;
- one image artifact;
- one build-script artifact;
- an array of source artifacts;
- one or more output artifacts, exposed as a bundle.

### Image

```nickel
let mImage = fun payload =>
  'Image {
    mode = payload.mode,
    base = payload.base,
    inputs = payload.inputs,
  }
```

Intended meaning:

- builder-specific configuration fields;
- optional or required base image artifact, depending on mode;
- an array of binary-output artifacts;
- one output artifact, usually a container image.

## Multi-Output Bundle Sketch

The preferred model is that a multi-output builder returns a bundle directly.

Conceptually:

```nickel
let zstdTerm = mBinary {
  outputs = ["out", "dev"],
  image = bootstrapImage,
  script = zstdScript,
  sources = [zstdSrc],
} in
{
  zstd = zstdTerm.out,
  zstd_dev = zstdTerm.dev,
}
```

The important part is not the exact `outputs = [...]` syntax, but the semantics:

- one builder term may declare several named outputs;
- the user receives a bundle with those names as fields;
- each field behaves like an ordinary artifact term.

## Typed Input Direction

The intent is to replace untyped input lists with builder-specific typed fields.

Conceptually:

```nickel
let BinaryPayload = {
  outputs | Array String,
  image | Artifact,
  script | Artifact,
  sources | Array Artifact,
}
```

This sketch does **not** yet encode artifact sub-kinds precisely. It only shows
the structural direction:

- image is not mixed into a generic string list;
- script is not mixed into a generic string list;
- source artifacts are grouped as source artifacts.

A later design iteration may refine this toward:

- `Artifact Image`
- `Artifact BuildScript`
- `Artifact SourceTree`

if Nickel typing and ergonomics make that practical.

## Package Set Sketch

Here is a larger pseudo-example:

```nickel
let rec pkgs = {
  bootstrapImage = mContainerImage {
    image = "docker.io/library/buildpack-deps:bookworm",
    digest = "sha256:...",
  },

  buildscriptAutotools = mText {
    artifact_kind = "build-script",
    source = "#!/usr/bin/env bash\n...",
  },

  zstdSrc = mFetch {
    url = [
      "https://github.com/facebook/zstd/archive/refs/tags/v1.5.7.tar.gz",
    ],
    hash = "sha256:...",
  },

  zstdTerm = mBinary {
    outputs = ["out", "dev"],
    image = pkgs.bootstrapImage,
    script = pkgs.buildscriptAutotools,
    sources = [pkgs.zstdSrc],
  },

  zstd = pkgs.zstdTerm.out,
  zstd_dev = pkgs.zstdTerm.dev,
} in
pkgs
```

Properties of this example:

- `pkgs` is a convenience composition layer;
- package field names are not identity;
- dependency edges come from nested artifact terms;
- the selected top-level artifact can be any projection, for example `pkgs.zstd`.

## Selected Closed Artifact Term

The Rust interpreter is expected to receive one selected closed artifact term.

Conceptually:

```nickel
import "./.mbuild/recipe.ncl"
```

or:

```nickel
import "./custom-entry.ncl"
```

Where the imported file is expected to evaluate to one selected closed artifact term,
for example:

```nickel
let rec pkgs = { ... } in pkgs.zstd
```

or:

```nickel
let rec pkgs = { ... } in pkgs.zstdTerm.dev
```

The interpreter does not need the whole package namespace as a runtime lookup table.
It only needs the selected closed term and the terms reachable from it.

## What This Sketch Intentionally Avoids

This sketch does not yet commit to:

- exact Nickel contracts or type aliases;
- exact syntax for bundle typing;
- exact syntax for registered open builder rows;
- exact Rust/Nickel value boundary;
- exact encoding of single-output builders versus one-field bundles.

Those should be decided only after the user-facing model is judged ergonomic enough.
