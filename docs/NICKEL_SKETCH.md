# Nickel Examples

## Package Composition

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

In this example:

- `mFetch`, `mText`, and `mBinary` are builder operations
- `pkgs.zstdSrc`, `pkgs.zstdScript`, and `pkgs.zstd` are object terms
- `pkgs.zstdTerm` is a multi-output bundle
- `.out` and `.dev` are output projections

## Builder Operations

### Fetch

```nickel
let mFetch = fun payload =>
  'Fetch {
    url = payload.url,
    hash = payload.hash,
  }
```

### Text

```nickel
let mText = fun payload =>
  'Text {
    kind = payload.kind,
    source = payload.source,
  }
```

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

### Image

```nickel
let mImage = fun payload =>
  'Image {
    mode = payload.mode,
    base = payload.base,
    inputs = payload.inputs,
  }
```

## Typed Inputs

```nickel
let BinaryPayload = {
  outputs | Array String,
  image | Object,
  script | Object,
  sources | Array Object,
}
```

## Build Request Example

```nickel
{
  meta = {
    name = "zstd",
  },
  build = let rec pkgs = { ... } in pkgs.zstd,
}
```

## Larger Example

```nickel
let rec pkgs = {
  bootstrapImage = mContainerImage {
    image = "docker.io/library/buildpack-deps:bookworm",
    digest = "sha256:...",
  },

  buildscriptAutotools = mText {
    kind = "build-script",
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
{
  meta = {
    name = "zstd",
  },
  build = pkgs.zstd,
}
```
