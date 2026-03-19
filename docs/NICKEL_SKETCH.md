# Nickel Examples

## Minimal Self-Contained Recipe

```nickel
store.text "hello-script" {
  kind = "build-script",
  source = "#!/usr/bin/env bash\necho hello\n",
}
```

This entry file evaluates to one primitive STORE action. `mbuild` evaluates the
file to that action and interprets it directly.

## Monadic Dependency Sequencing

```nickel
store.bind (store.fetch "bash-src-5.3" {
  url = ["https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz"],
  hash = "sha256:...",
}) (fun bashSrc =>
store.bind (store.text "buildscript-bash-stage2" {
  kind = "build-script",
  source = "#!/usr/bin/env bash\n...",
}) (fun bashScript =>
store.bind (store.container_image "bootstrap-image" {
  image = "docker.io/library/buildpack-deps:bookworm",
  digest = "sha256:...",
}) (fun bootstrapImage =>
store.binary "bash-stage2" {
  optimize = "size",
} bootstrapImage bashScript [bashSrc])))
```

In this example:

- the entry file itself decides what to build
- `mbuild` does not select a package field
- dependency recursion is expressed through `bind`
- `binary` receives already-realized `Build` values

## Reading Builder-Generated Metadata

Because continuations receive realized `Build` values, Nickel code can inspect
builder-generated metadata before constructing the next action:

```nickel
store.bind (store.container_image "bootstrap-image" {
  image = "docker.io/library/buildpack-deps:bookworm",
  digest = "sha256:...",
}) (fun bootstrapImage =>
  let imageRef = bootstrapImage.attrs.image_ref in
  let imageDigest = bootstrapImage.attrs.image_digest in
  store.text "bootstrap-info" {
    kind = "text-file",
    source = "ref=" ++ imageRef ++ "\ndigest=" ++ imageDigest ++ "\n",
  })
```

Builder-generated metadata is available through `Build` values, not through
human-facing refs.

## STORE Library Sketch

Conceptually, a Nickel STORE helper module looks like:

```nickel
{
  return = fun x => 'Return x,

  bind = fun action cont =>
    'Bind { action = action, cont = cont },

  map = fun f action =>
    'Bind { action = action, cont = fun x => 'Return (f x) },

  sequence = fun actions =>
    std.array.fold_right
      (fun action acc =>
        'Bind {
          action = action,
          cont = fun x =>
            'Bind {
              action = acc,
              cont = fun xs => 'Return ([x] ++ xs),
            },
        })
      ('Return [])
      actions,

  text = fun name payload =>
    'Text { name = name, payload = payload },

  fetch = fun name payload =>
    'Fetch { name = name, payload = payload },

  container_image = fun name payload =>
    'ContainerImage { name = name, payload = payload },

  binary = fun name payload image script sources =>
    'Binary {
      name = name,
      payload = payload,
      image = image,
      script = script,
      sources = sources,
    },
}
```

Rust interprets these action variants and returns realized `Build` records.

## `Build` Shape

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
