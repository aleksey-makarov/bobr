# Build logging

`bobr` records the progress of a build as structured events. This document
fixes the contract: the channels, the on-disk layout, the event record, the
closed `status` vocabulary, and the format guarantees tooling may rely on.

## Channels

There are three output channels with distinct purposes:

- **Run logs** (the request's `logs` directory) are the single source of truth
  for a run, for both humans (rendered) and machines (JSONL). Durable
  build-significant events go here; transient progress is screen-only.
  Diagnostics before logging is initialized and logging failures may appear
  only on stderr. The caller names the directory and creates it; by convention
  it is `<store>/logs/<run-id>` (see
  [Request](./REQUEST.md)).
- **stderr** carries the live UI and human diagnostics. The progress renderer
  projects structured events onto the screen; CLI diagnostics and logging-error
  notices may also write there. In an interactive terminal (and not `quiet`) it
  draws a **live block** (run summary, fetch/build statistics, then a bounded
  activity viewport, with warnings/errors printed above) via `indicatif`;
  otherwise (non-TTY, e.g. CI or a pipe, or `quiet`) it falls back to
  **plain per-line** output.
  Transient `progress` ticks appear only in the live block — the plain path
  omits them. How much it shows is a threshold (see
  [Levels and verbosity](#levels-and-verbosity)).
- **stdout** carries the machine-readable result: a single goal prints its
  `ObjectHash`, while multiple goals print their ordered JSON results. Moving
  the result into a store file is a related, separate concern and is not part
  of the logging contract.

## Live viewport

The request's `progress` policy controls only an interactive TTY. Every live
block has this fixed order:

```text
run    <whole-run summary>
fetch  <Source and repository acquisition statistics>
build  <builder statistics>
<N activity rows>
```

The three status rows always exist; activity rows are additional. Their budget
is stable across ordinary activity events and is recomputed on terminal resize.

- `auto` caps activity rows at the greater of `jobs` and four, and at three
  quarters of the terminal height. The complete block also leaves at least two
  terminal rows free when space permits. Small terminals can reduce activity
  capacity below four;
- `fixed` caps the complete block at `max_lines` rows (minimum four), with the
  same terminal-space and concurrency caps;
- `summary` keeps the three fixed rows and hides individual activity rows.

An activity row represents a concrete builder, network acquisition, repository
copy, or batched fs-file hardlink import. Empty rows display `—`. Local Path
Sources, working-store hits, and short ordinary-object hardlinks update fetch
statistics without taking a row. A potentially long fs-file hardlink import
takes one stable row for the batch, not one row per linked file. A network Source
waiting for a connection slot updates only fetch statistics. Network activities
take a row at their first `running` network milestone. Repository copies take a
row when copying starts. Remote metadata and content transfers use the same
activity viewport.

The renderer keeps every activity in its model even when only part fits on
screen. There are separate FIFO queues for hidden builders and acquisitions.
When a row becomes free, the oldest hidden builder takes it; if there is none,
the oldest hidden acquisition does. The policy affects display only, never
execution. A visible failed builder remains until the run finishes. Network
activities with `details.network_lifecycle = true` release their row on failure
so a failed transfer does not occupy it throughout fallback to another provider;
the diagnostic remains above the live block and in the log.

The `fetch` row reports current `transferring`, connection-slot `waiting`,
`complete`, transient `retrying`, and `failed` counts. The `build` row reports
workers `running`, jobs `waiting` in the bounded BuildExecutor FIFO,
`complete`, and `failed`. The `run` row reports whole-run outcomes plus the
number of builders and acquisitions hidden only by the viewport.

The initial acquisition total is the number of reachable Source nodes.
`SecondaryContent` and `RepositoryMetadata` activities extend it when first
observed, including individual remote fs-file transfers. Lazy resolution cannot
know all repository work at run start. Acquisition counts are therefore not
counts of distinct built objects or just upstream source downloads.

The number of visible rows never limits builder execution. A Tokio task listens
for `SIGWINCH` and immediately asks the logger to reflow the viewport; build
events update its content independently. On resize, visible activities keep
their order as far as possible; removed rows return to their kind's FIFO and
new rows are filled with the same builder-first rule. Non-TTY and `quiet`
output never emit terminal control sequences.

## On-disk layout

```text
<logs>/
  context.json                         optional driver-owned run provenance
  recipe-catalog.json                  optional driver-owned name/tag catalog
  events.jsonl                         run-level event log (the full audit log)
  index.jsonl                          workspace allocation index
  <serial>-<tag>[-<name>]/             one directory per built/materialized subject
    meta.json                          subject identity and paths
    events.jsonl                       this subject's events only
    raw/                               raw logs (step stdout/stderr, reports, …)
```

The run-level `events.jsonl` is created exclusively: a run given a log directory
another run already wrote to fails at once, rather than appending to someone
else's audit trail. It is the full audit log of the run: it contains run-level
events plus a copy of every bound subject event. Each bound subject's own
`events.jsonl` contains only that subject's events. Its run-level and
subject-level copies are byte-identical, so tooling can match them.

Events may carry a subject identity without being bound to a subject workspace.
Cache hits and repository network events use this form: they appear only in the
run-level log and do **not** create a subject directory or `subject_seq`.

`context.json` and `recipe-catalog.json` are not part of the event protocol and
are not written by the Bobr logger. The standard recipes driver places them
beside the event log so inspection tools can associate a store mapping with the
Bobr/recipes versions and human-facing recipe labels observed by that run.

## Event record

Each line of `events.jsonl` is one JSON object. The producer (builder,
scheduler) supplies only the *payload*; the logger adds the *envelope*, so a
producer can neither forge nor omit envelope fields.

```jsonc
{
  "schema": "bobr-build-event-v1",   // format version
  "seq": 412,                        // monotonic per run; primary ordering
  "subject_seq": 3,                  // monotonic per bound subject; optional
  "ts": "2026-06-23T21:21:51.229Z",  // UTC, RFC3339, milliseconds, always 'Z'
  "level": "info",                   // info | warn | error
  "status": "done",                  // closed lifecycle enum (see below)
  "op": "sandbox",                   // optional, free-form builder operation
  "subject": {                       // optional identity, even without a workspace
    "tag": "Sandbox",
    "name": "qemu-image",
    "build_key": "<full build key>",
    "object_hash": "<full object hash>"   // optional, when an object identity is known
  },
  "message": "…",
  "raw_log": "00000009-Sandbox-bash/raw/sandbox-result.log",  // optional, run-relative
  "details": { }                     // optional
}
```

Field notes:

- **`run_id` is not a field.** The request names the run, and by convention its
  log directory is named after it; the id is constant for the whole run, so it is
  not repeated per line.
- **`seq`** is the primary order: a run-global monotonic counter, stamped once
  when the event is emitted. It is reliable regardless of timestamp granularity
  or parallelism. **`ts`** is secondary.
- **`subject_seq`** orders a single subject's events; it is present only on
  events bound to a subject.
- **`subject.build_key`** contains the full BuildKey for recipe subjects. For
  synthetic repository activities, the historical field name holds an opaque
  activity identifier, which is not necessarily a hash. Tooling must use the
  subject tag and event context rather than parse every such value as BuildKey.
- **`object_hash`**, when present, contains the full ObjectHash. Display text and
  synthetic activity names may include 12-character prefixes; those are not
  substitutes for full identities.
- **`raw_log`** is relative to the run directory; the stderr renderer rejoins
  the run directory to show an absolute path.

## `status` (closed) vs `op` (free-form)

`status` is a fixed, closed enum — the lifecycle axis tooling filters on. `op`
is an optional, free-form builder operation. They are separate fields so that
filtering by lifecycle is reliable while builders stay free to name their work.

`status` values:

| value          | meaning                                                        |
|----------------|----------------------------------------------------------------|
| `run-started`  | run-level: realization started (goals, jobs, reachable counts, progress and repository policies) |
| `run-finished` | run-level: realization finished (goals or error class, counters) |
| `start`        | subject execution started                                      |
| `cache-miss`   | no reusable result at this point; acquisition or execution follows |
| `running`      | builder/source work or repository acquisition is running       |
| `cache-hit`    | subject served from cache (no workspace; run-level)            |
| `done`         | recipe or acquisition completed; `object_hash` is optional     |
| `failed`       | subject or run failed                                          |
| `cancelled`    | cancelled                                                      |
| `cleanup`      | post-execution cleanup (e.g. temp-dir removal warning)         |

Builder and Source operations ride inside `running` and name themselves with
`op`; publication and other lifecycle events may also carry it. Examples include
`sandbox`, `fetch`, `publish`, and `compose`. `op` is intentionally open;
tooling must not assume a closed set.

## Run-level events

The run-level `events.jsonl` is a full audit log, not just an aggregate of
subject events. Beyond the fanned-out subject events it carries:

- `run-started`: ordered goals, `jobs`, total reachable nodes, reachable
  builders and Sources, the selected progress policy, and ordered providers.
  Each provider records its name, capability, and backend kind: local backends
  include the canonical store path and content transfer mode, remote backends
  the master URL. Signing-key contents, credentials, and authorization headers
  are not included; there is no legacy `trusted` flag;
- `cache-hit`: one per subject served from a reusable object during realization
  (carries the subject identity and `object_hash`), including reuse discovered
  within the current run. A fully exact-cached run records only the resolved
  boundary, not pruned interior subtrees;
- `run-finished`: realized goals or `details.error_class`, plus exact terminal
  counters: `built`, `cache_hit`, `failed`, `cancelled`, `downloaded`, `local`,
  `secondary`, `hardlinked`, `copied`, `remote`, and `already_present`.
  `hardlinked`, `copied`, and `remote` count completed known-object acquisitions
  using hardlink, copy, or download transport, respectively; they are not counts
  of individual fs-files or HTTP requests. A mixed fs-tree closure can contribute
  to several transport counters. Retry totals and logging failures are included
  when present.

Source terminal/cache events carry `details.source_outcome` with one of
`downloaded`, `local`, `secondary`, or `already_present`. Network milestones
carry `details.transfer = "network"`, `host`, and optional byte counters; local
materialization uses `transfer = "local"`.

Trusted mapping and content provenance use separate events and fields. A
`secondary-build` or `secondary-reuse` run event carries
`details.mapping_providers`, each with the configured provider name and asserted
`object_hash`; it never claims that the same repository supplied bytes.
Physical repository acquisition uses a synthetic `SecondaryContent` subject.
Its `running` milestones carry `content_provider`, `transfer_mode`, and byte,
file, and duration measurements. The terminal `repository-content` event
carries the aggregate `content_providers`, `content_outcomes`, `files`, `bytes`,
`encoded_bytes`, `decoded_bytes`, and `duration_ms`. Transfer-mode values are
`hardlink`, `copy`, or `download`. This records split closures accurately when the
manifest and its fs-files come from different repositories or use different
transports.

### Remote network events

Repository network lifecycle events have `op = "repository-metadata"` or
`"repository-content"` and `details.network_lifecycle = true`. Their synthetic
subject tags are `RepositoryMetadata` for master/index/list requests and
`SecondaryContent` for object/fs-file requests. They are run-level-only events,
not one allocated workspace per downloaded fs-file.

Details identify `repository`, `repository_class`, `repository_purpose`, `host`,
and `transfer = "network"`. Start/progress/completion records carry attempt and
byte measurements when available; retries carry `retry_host`, `retry_delay_ms`,
and `retry_reason`. Cancellation is reported with `status = "cancelled"`.
Network byte measurements describe encoded response bodies; aggregate content
events distinguish them from decoded content sizes.

Expected lookup misses do not produce warning/error diagnostics or their own
activity rows. Failure to retrieve or verify advertised content is reported
separately and may be followed by acquisition from another provider.

## Levels and verbosity

Levels are `progress`, `info`, `warn`, `error` (ordered
`progress < info < warn < error`).

`progress` is a **transient, screen-only** level for high-frequency ticks
(e.g. download byte counts). It appears only in the interactive live block, and
is **never persisted** — `FileSink` drops it, so the on-disk vocabulary is only
`info`/`warn`/`error`, and progress ticks do not consume the durable `seq`
(no gaps in the file). Use `info` for durable milestones worth keeping (e.g.
"fetching X" / "fetched N bytes"); use `progress` for the noisy in-between.

`quiet` (a boolean request setting) raises the **stderr threshold**, rather than
being an on/off switch:

- `quiet = true`: only `warn`/`error` reach stderr; `info` and `progress` are
  silenced.
- otherwise: `info`, `warn`, and `error` reach stderr, plus — in the interactive
  live block — transient `progress` ticks.

`warn`/`error` are never suppressed on stderr. The threshold affects only
stderr; **file logs always record every persisted level** (`info`/`warn`/
`error`) regardless of `quiet` — `progress` is screen-only by design.

## Guarantees

- **Best-effort.** A failed log write prints a warning to stderr but never fails
  the build. Such failures are counted and reported as `logging_errors` in the
  `run-finished` event's `details` (a write failure of `run-finished` itself is
  not reflected in its own count).
- **Durability.** File logs are buffered: routine `info` events are not flushed
  per event (fewer syscalls). `warn`/`error` events and the terminal
  `run-finished` event are flushed immediately so anything diagnostically
  relevant survives a process crash; `run-finished` additionally fsyncs the run
  log so a completed run is durable across power loss. Remaining buffered events
  are flushed when the logger is dropped (normal exit or panic-unwind); a
  `SIGKILL` can lose the unflushed tail.
- **Ordering.** `seq` is authoritative for run order; `ts` is informational.
  Per-subject order is `subject_seq`.
- **Timestamps** are honest UTC (RFC3339, milliseconds, trailing `Z`), never
  local time.
- **Portability.** `raw_log` paths are relative to the run directory, so logs
  survive a moved store.
- **Schema.** `schema` is bumped when the record format changes; readers should
  check it.
