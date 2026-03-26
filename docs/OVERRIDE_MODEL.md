# Override Semantics

## Summary

Override is a Nickel-level operation.

Rust does not implement override as a runtime primitive. Rust embeds Nickel,
evaluates the overridden STORE program, and interprets the resulting STORE
actions.

## Core Rule

Override transforms recipe definitions, helper values, or primitive builder
helper arguments in Nickel.

It does not directly manipulate:

- object hashes
- build records
- publication refs
- store paths

## What Override Can Change

Override may change:

- builder payload fields
- dependency sequencing
- explicit publication names
- helper values used to construct STORE actions

Examples:

```nickel
pkgs // {
  bashStage2 = mkBashStage2 {
    optimize = "size",
  }
}
```

```nickel
pkgs // {
  bashStage2 = mkBashStage2 {
    baseImageAction = pkgs.altBootstrapImage,
  }
}
```

```nickel
pkgs // {
  bashStage2 = mkBashStage2 {
    name = "bash-stage2-debug",
  }
}
```

## Runtime Interaction

Operationally:

1. Nickel evaluates package definitions and overrides.
2. Nickel produces one top-level STORE action.
3. Rust interprets the resulting STORE action tree.
4. Rust computes public `build_key` from builder tag, normalized payload, and ordered
   `input_build_keys`, and canonical `result_key` from ordered
   `input_object_hashes`.
5. Rust reuses or executes builders.
6. Rust implicitly updates publication refs from the explicit name carried by
   each primitive builder action.

## Effect on Identity

Changing builder payload or dependencies changes the resulting `build_key`.

Changing only the explicit publication name does not change `build_key`. It only
changes which human-facing refs are updated.
