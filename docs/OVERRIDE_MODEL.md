# Override Semantics

## Summary

Override is a Nickel-level operation.

Rust does not implement override as a runtime primitive. Rust embeds Nickel,
evaluates the overridden builder program, and interprets the resulting STORE
operations.

## Core Rule

Override transforms package definitions, helper values, or builder arguments in
Nickel.

It does not directly manipulate:

- object hashes
- build records
- publication refs
- store paths

## What Override Can Change

Override may change:

- builder payload fields
- dependency selection
- explicit publication names
- helper values used to construct builder calls

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
    image = pkgs.altBootstrapImage,
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
2. Rust interprets the resulting primitive STORE operations.
3. Rust computes `build_key` from builder tag, normalized payload, and ordered
   `input_build_keys`.
4. Rust reuses or executes builders.
5. Rust implicitly updates publication refs from the explicit name argument of
   each primitive builder call.

## Effect on Identity

Changing builder payload or dependencies changes the resulting `build_key`.

Changing only the explicit publication name does not change `build_key`. It only
changes which human-facing refs are updated.
