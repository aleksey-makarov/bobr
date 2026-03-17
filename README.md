# mbuild

`mbuild` executes one Nickel recipe entry file.

The entry file evaluates to a top-level STORE action. Rust embeds Nickel,
evaluates the entry file to weak head normal form, and then interprets the
resulting STORE action tree step by step.

STORE programs are built from:

- `return`
- `bind`
- named primitive builder actions such as `text`, `fetch`, `binary`, and
  `image`

Primitive builder actions do not recursively execute dependency actions on their
own. Dependency sequencing is expressed in Nickel through monadic combinators.
A primitive builder action consumes already-realized `Build` values and returns
one `STORE Build` action.

A realized `Build` value is the canonical build record stored under
`.mbuild/builds/<build_key>.json`.

Realized results are stored in a local content-addressed store where object
identity is determined only by payload content. Builder invocations are recorded
separately under stable build keys, and published names resolve through
metadata refs and object refs. Hashing, build recording, dependency resolution,
and publication are interpreter details rather than part of the user-facing
Nickel API.

For the architecture documents, start with [`docs/INDEX.md`](./docs/INDEX.md).
