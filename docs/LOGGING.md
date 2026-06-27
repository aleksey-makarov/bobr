# Build logging

`bobr` records the progress of a build as structured events. This document
fixes the contract: the channels, the on-disk layout, the event record, the
closed `status` vocabulary, and the format guarantees tooling may rely on.

## Channels

There are three non-overlapping output channels:

- **Store logs** (`<store>/logs/<run-id>/…`) are the single source of truth for
  a run, for both humans (rendered) and machines (JSONL). Everything
  build-significant goes here, at every level.
- **stderr** is the live UI only: build progress plus warnings and errors, as a
  projection of the store logs onto the screen. The progress renderer is the
  only writer of stderr. How much it shows is a threshold (see
  [Verbosity](#verbosity)).
- **stdout** carries the machine-readable result (the realized object JSON).
  Moving the result into a store file is a related, separate concern and is not
  part of the logging contract.

## On-disk layout

```text
<store>/logs/<run-id>/
  events.jsonl                         run-level event log (the full audit log)
  index.jsonl                          workspace allocation index
  <serial>-<tag>[-<name>]/             one directory per built/materialized subject
    meta.json                          subject identity and paths
    events.jsonl                       this subject's events only
    raw/                               raw logs (step stdout/stderr, reports, …)
```

The run-level `events.jsonl` is the full audit log of the run: it contains
run-level events plus a copy of every subject event. Each subject's own
`events.jsonl` contains only that subject's events. A subject event's run-level
and subject-level copies are byte-identical, so tooling can match them.

Cache hits and other run-level events do **not** create a subject directory.

## Event record

Each line of `events.jsonl` is one JSON object. The producer (builder,
scheduler) supplies only the *payload*; the logger adds the *envelope*, so a
producer can neither forge nor omit envelope fields.

```jsonc
{
  "schema": "bobr-build-event-v1",   // format version
  "seq": 412,                        // monotonic per run; primary ordering
  "subject_seq": 3,                  // monotonic per subject; only on subject events
  "ts": "2026-06-23T21:21:51.229Z",  // UTC, RFC3339, milliseconds, always 'Z'
  "level": "info",                   // info | warn | error
  "status": "done",                  // closed lifecycle enum (see below)
  "op": "mkfs",                      // optional, free-form builder operation
  "subject": {                       // omitted for run-level events
    "tag": "Erofs",
    "name": "erofs-rootfs",
    "build_key": "<full build key>",
    "object_hash": "<full object hash>"   // optional (present on completion/cache hit)
  },
  "message": "…",
  "raw_log": "00000009-Sandbox-bash/raw/sandbox-result.log",  // optional, run-relative
  "details": { }                     // optional
}
```

Field notes:

- **`run_id` is not a field.** It is the run directory name
  (`logs/<run-id>/`), constant for the whole run, so it is not repeated per
  line.
- **`seq`** is the primary order: a run-global monotonic counter, stamped once
  when the event is emitted. It is reliable regardless of timestamp granularity
  or parallelism. **`ts`** is secondary.
- **`subject_seq`** orders a single subject's events; it is present only on
  events bound to a subject.
- **`build_key` and `object_hash` are the full values.** A 12-character short
  form is derived by truncation for the live screen line only; it is not stored.
- **`raw_log`** is relative to the run directory; the stderr renderer rejoins
  the run directory to show an absolute path.

## `status` (closed) vs `op` (free-form)

`status` is a fixed, closed enum — the lifecycle axis tooling filters on. `op`
is an optional, free-form builder operation. They are separate fields so that
filtering by lifecycle is reliable while builders stay free to name their work.

`status` values:

| value          | meaning                                                        |
|----------------|----------------------------------------------------------------|
| `run-started`  | run-level: build started (root, jobs, subject count, backend)  |
| `run-finished` | run-level: build finished (`details.result` = ok/failed/cancelled, counters) |
| `start`        | subject execution started                                      |
| `cache-miss`   | subject not cached; will be built                              |
| `running`      | subject's builder/source implementation is running             |
| `cache-hit`    | subject served from cache (no workspace; run-level)            |
| `done`         | subject completed; carries `object_hash`                       |
| `failed`       | subject or run failed                                          |
| `cancelled`    | cancelled                                                      |
| `cleanup`      | post-execution cleanup (e.g. temp-dir removal warning)         |

Builder operations ride inside `running` and name themselves with `op`. Current
values: `stage`, `mkfs`, `merge`, `subset`, `extract`, `initramfs`, `sandbox`,
`sandbox-result`, plus the meta operations `log-warning` and
`oci-extract-warnings`. `op` is intentionally open; tooling must not assume a
closed set.

## Run-level events

The run-level `events.jsonl` is a full audit log, not just an aggregate of
subject events. Beyond the fanned-out subject events it carries:

- `run-started`: root key/name/tag, `jobs`, subject count, runtime backend
  (`host`/`namespace`);
- `cache-hit`: one per cached subject resolved while planning (carries the
  subject identity and `object_hash`); a fully cached run records only the
  resolved boundary, not pruned interior subtrees;
- `run-finished`: `details.result` (`ok`/`failed`/`cancelled`) and the
  `built`/`cache_hit`/`failed` counters.

## Levels and verbosity

Levels are `progress`, `info`, `warn`, `error` (ordered
`progress < info < warn < error`).

`progress` is a **transient, screen-only** level for high-frequency ticks
(e.g. download byte counts). It is rendered to stderr in non-quiet mode but
**never persisted** — `FileSink` drops it, so the on-disk vocabulary is only
`info`/`warn`/`error`, and progress ticks do not consume the durable `seq`
(no gaps in the file). Use `info` for durable milestones worth keeping (e.g.
"fetching X" / "fetched N bytes"); use `progress` for the noisy in-between.

`quiet` (a boolean request setting) is a **stderr level threshold**, not
an on/off switch:

- `quiet = true`: only `warn`/`error` reach stderr; `progress` and `info`
  are silenced.
- otherwise: everything from `progress` up reaches stderr.

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
