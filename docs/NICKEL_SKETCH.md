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
  let image = bootstrapImage.attrs.image in
  let imageId = bootstrapImage.attrs.image_id in
  store.text "bootstrap-info" {
    kind = "text-file",
    source = "image=" ++ image ++ "\nimage-id=" ++ imageId ++ "\n",
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
      (fun act next =>
        'Bind {
          action = act,
          cont = fun x =>
            'Bind {
              action = next,
              cont = fun xs => 'Return ([x] @ xs),
            },
        })
      ('Return [])
      actions,

  sequence_ = fun actions =>
    std.array.fold_right
      (fun act next =>
        'Bind {
          action = act,
          cont = fun _ =>
            next,
        })
      ('Return null)
      actions,

  for_each = fun items f =>
    std.array.fold_right
      (fun item next =>
        'Bind {
          action = f item,
          cont = fun _ =>
            next,
        })
      ('Return null)
      items,

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
  build_key = "0123456789abcdef...",
  object_hash = "fedcba9876543210...",
  kind = "container-image",
  attrs = {
    image = "docker.io/library/buildpack-deps:bookworm",
    image_id = "sha256:...",
  },
}
```
