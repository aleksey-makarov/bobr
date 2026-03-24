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

By default, `mbuild` prints concise live build progress to `stderr`. The final
result still goes to `stdout` only. Use `--quiet` to suppress live progress.

A realized `Build` value is the canonical build record stored under
`.mbuild/builds/<build_key>.json`. Build records carry a persistent
`created_at` timestamp in RFC3339 UTC format. This timestamp does not affect
`build_key`.

Realized results are stored in a local content-addressed store where object
identity is determined only by payload content. Builder invocations are recorded
separately under stable build keys, and published names resolve through
metadata refs and object refs. The plain published name is always the current
alias. Republishing a different build under the same name rotates the previous
current refs into timestamp-suffixed history refs. Hashing, build recording,
dependency resolution, and publication are interpreter details rather than part
of the user-facing Nickel API.

Each `mbuild` invocation also writes persistent logs:

- one structured event log under `.mbuild/logs/runs/<YYMMDDHHMMSS>-<pid>.jsonl`
- raw external-command logs under
  `.mbuild/builder-state/<builder>/logs/<name>/`

The event log records build lifecycle events such as `start`, `cache-hit`,
`cache-miss`, `run`, `publish`, `done`, and `fail`. For readability, the
top-level `build_key` and `object_hash` fields in the event log are shortened;
the full identifiers remain available in the event `details`. Raw logs contain
the captured stdout/stderr of external commands such as `podman run`,
`podman import`, or `podman inspect`.

For the architecture documents, start with [`docs/INDEX.md`](./docs/INDEX.md).
