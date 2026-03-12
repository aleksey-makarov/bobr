# mbuild

`mbuild` is an experimental term-centric build system with modular builder components.

Users describe builds in Nickel as pure compositions of typed builder terms and
multi-output bundles, while the `mbuild` runtime interprets one selected closed
term from `./.mbuild/recipe.ncl` or another entry file. Builder operations are
extensible through a runtime registry, and Nix-like package-level override stays
in the Nickel layer instead of leaking into the execution model.

Realized results are stored in a local content-addressed store that separates
payload objects from artifact metadata and keeps builder-specific runtime state
under per-builder directories in `.mbuild/`. Hashing, caching, dependency
resolution, and store publication are interpreter details rather than part of
the user-facing Nickel API.

For the current architecture notes and design drafts, start with
[`docs/INDEX.md`](./docs/INDEX.md).
