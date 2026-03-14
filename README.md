# mbuild

`mbuild` is a term-centric build system with modular builder components.

Users describe builds in Nickel as pure compositions of typed builder terms and
multi-output bundles. The `mbuild` runtime interprets one selected build request
from `./.mbuild/recipe.ncl` or another entry file. Builder operations are
extensible through a runtime registry, and package-level override stays in the
Nickel layer instead of leaking into the execution model.

Realized results are stored in a local content-addressed store where object
identity is determined only by payload content. Builder invocations are recorded
separately under stable build keys, while published names resolve through
metadata refs and object refs. Hashing, build recording, dependency resolution,
and store publication are interpreter details rather than part of the user-facing
Nickel API.

For the architecture documents, start with [`docs/INDEX.md`](./docs/INDEX.md).
