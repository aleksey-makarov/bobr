# Nickel Examples

## Package Composition

```nickel
let rec pkgs = {
  bootstrapImage = mContainerImage "bootstrap-image" {
    image = "docker.io/library/buildpack-deps:bookworm",
    digest = "sha256:...",
  },

  bashSrc = mFetch "bash-src-5.3" {
    url = [
      "https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz",
      "https://mirrors.kernel.org/gnu/bash/bash-5.3.tar.gz",
    ],
    hash = "sha256:...",
  },

  bashScript = mText "buildscript-bash-stage2" {
    kind = "build-script",
    source = "#!/usr/bin/env bash\n...",
  },

  bashStage2 = mBinary "bash-stage2" {
    optimize = "size",
  } pkgs.bootstrapImage pkgs.bashScript [pkgs.bashSrc],
} in
pkgs
```

In this example:

- every primitive builder call carries an explicit publication name
- every primitive builder call evaluates to a `Built` value
- downstream calls consume upstream `Built` values directly

## Accessing Builder-Generated Metadata

```nickel
let rec pkgs = {
  bootstrapImage = mContainerImage "bootstrap-image" {
    image = "docker.io/library/buildpack-deps:bookworm",
    digest = "sha256:...",
  },

  bootstrapDigest = pkgs.bootstrapImage.attrs.image_digest,
  bootstrapRef = pkgs.bootstrapImage.attrs.image_ref,
} in
pkgs
```

Builder-generated metadata is available through `Built` values, not through
human-facing refs.

## Builder Operations

### Fetch

```nickel
let mFetch = fun name => fun payload =>
  builtin.fetch name payload
```

### Text

```nickel
let mText = fun name => fun payload =>
  builtin.text name payload
```

### Binary

```nickel
let mBinary = fun name => fun payload => fun image => fun script => fun sources =>
  builtin.binary name payload image script sources
```

### Image

```nickel
let mImage = fun name => fun payload => fun base => fun inputs =>
  builtin.image name payload base inputs
```

## `Built` Shape

Conceptually, a realized value has the same shape as one build record:

```nickel
{
  build_key = "sha256:...",
  object_hash = "sha256:...",
  kind = "container-image",
  attrs = {
    image_ref = "docker.io/...@sha256:...",
    image_digest = "sha256:...",
  },
}
```

## Bash Stage 2 Sketch

```nickel
let rec pkgs = {
  bootstrapImage = mContainerImage "bootstrap-image" {
    image = "docker.io/library/buildpack-deps:bookworm",
    digest = "sha256:...",
  },

  bashSrc = mFetch "bash-src-5.3" {
    url = ["https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz"],
    hash = "sha256:...",
  },

  bashScript = mText "buildscript-bash-stage2" {
    kind = "build-script",
    source = "#!/usr/bin/env bash\n...",
  },

  bashStage2 = mBinary "bash-stage2" {
    optimize = "size",
  } pkgs.bootstrapImage pkgs.bashScript [pkgs.bashSrc],
} in
pkgs.bashStage2
```
