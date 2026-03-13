# mbuild

`mbuild` is an experimental term-centric build system with modular builder components.

Users describe builds in Nickel as pure compositions of typed builder terms and
multi-output bundles, while the `mbuild` runtime interprets one selected build
request from `./.mbuild/recipe.ncl` or another entry file. Builder operations are
extensible through a runtime registry, and Nix-like package-level override stays
in the Nickel layer instead of leaking into the execution model.

Realized results are stored in a local content-addressed store where identity is
determined only by payload content. Technical metadata and publication metadata
are kept separately and do not participate in identity, while builder-specific
runtime state lives under per-builder directories in `.mbuild/`. Hashing,
caching, dependency resolution, and store publication are interpreter details
rather than part of the user-facing Nickel API.

For the current architecture notes and design drafts, start with
[`docs/INDEX.md`](./docs/INDEX.md).
