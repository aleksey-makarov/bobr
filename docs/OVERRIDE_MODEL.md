# Override Semantics

## Summary

Override is a Nickel-level operation.

Rust does not implement override as a runtime primitive. Rust receives the final
selected build request after all overrides have been applied.

## Core Rule

Override transforms package definitions or builder arguments in Nickel.

It does not manipulate:

- object hashes
- build keys
- store refs
- object records
- build records

## Package Shape

A package is parameterized by arguments and can be reinstantiated with changed
arguments.

Example:

```nickel
mkPackage = fun defaults =>
  {
    args = defaults,
    outputs = buildWith defaults,
    override = fun patch =>
      mkPackage (defaults & patch),
  }
```

`override` merges a patch into the package arguments and rebuilds the package
value from those arguments.

## Forms of Override

### Replace a package binding

```nickel
pkgs // { zstd = myZstd }
```

### Override builder configuration

```nickel
pkgs.zstd.override {
  optimize = "size",
}
```

### Override dependencies

```nickel
pkgs.zstd.override {
  image = pkgs.altBootstrapImage,
  script = pkgs.altBuildScript,
}
```

## Multi-Output Bundles

Override applies to the package or builder term that produces the bundle, not to
already projected outputs.

```nickel
let zstdPkg = mkZstdPackage { ... } in
let zstdAlt = zstdPkg.override { image = pkgs.altImage } in
{
  zstd = zstdAlt.outputs.out,
  zstd_dev = zstdAlt.outputs.dev,
}
```

## Runtime Interaction

Operationally:

1. Nickel evaluates package definitions and overrides.
2. Nickel produces one selected build request.
3. Rust interprets that request recursively.
4. Rust computes build keys and object hashes and performs lookup.
5. Rust executes builders only when needed.

Override affects runtime behavior indirectly by changing the final selected build
request.
