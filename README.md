# mbuild

`mbuild` is a term-centric build system with modular builder components.

Users describe builds in Nickel. Rust embeds Nickel and interprets primitive
STORE builder operations. Each primitive builder call carries an explicit
publication name, evaluates to a realized `Built` value, and implicitly updates
human-facing refs for that name.

Realized results are stored in a local content-addressed store where object
identity is determined only by payload content. Builder invocations are recorded
separately under stable build keys, and published names resolve through
metadata refs and object refs. Hashing, build recording, dependency resolution,
and store publication are interpreter details rather than part of the user-
facing Nickel API.

For the architecture documents, start with [`docs/INDEX.md`](./docs/INDEX.md).
