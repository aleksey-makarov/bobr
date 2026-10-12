# Scheduler

This document describes the realization scheduler implemented by `bobr`. It
covers DAG traversal, cache and reuse resolution, Source acquisition, builder
execution, concurrency limits, and cancellation. It describes the current
algorithm, including its deliberately lazy behavior and its known scheduling
consequences.

The scheduler is not one global queue of topologically ready nodes. It is a
demand-driven set of asynchronous computations coordinated by the
`DynamicRealizer`, with separate executors for Source acquisition and
synchronous builders.

## Components

One realization run has these cooperating components:

- the `DynamicRealizer` traverses the planned DAG, resolves object identities,
  decides whether content or a build is required, and coordinates all goals;
- the Source acquisition engine obtains pinned `Path`, `Http`, and
  `OciRegistry` content;
- the asynchronous `SecondaryResolver` separates explicitly enabled mapping
  providers from content providers and verified import;
- the shared `NetworkScheduler` bounds Source and remote-repository traffic
  with one global and per-host budget;
- the shared `LocalIoScheduler` bounds local provider operations, Source
  materialization/import, and builder input preparation;
- the `BuildExecutor` runs synchronous builders under the `jobs` limit.

Their relationship is:

```text
request goals
    |
    v
DynamicRealizer
    |-- mapping lookup --------------> SecondaryResolver
    |-- known-hash content lookup ---> SecondaryResolver
    |-- Source origin ---------------> Source acquisition engine
    `-- ready BuilderJob ------------> BuildExecutor
                                           |
                                           `-- staged output -> publication
```

Source network acquisition and remote providers share the `NetworkScheduler`.
Local provider adapters and selected blocking filesystem operations share the
`LocalIoScheduler`.

Mapping lookup, content acquisition, and builder execution are separate
operations. An enabled mapping provider can supply an `ObjectHash` without
providing the bytes of that object, and the hash can participate in a parent
`ReuseKey` before its content is present in the working store.

Before realization, bobr validates the ordered provider configuration and opens
its backends. Mapping and content providers are separate projections of that
list; each projection retains request order. Complementary capabilities share
one backend, identified by canonical store root for local providers or master
URL for remote providers. Repeating the same backend capability is rejected.

Every configured local repository is opened and its root canonicalized.
Hardlink repositories are rejected at this boundary unless both their
`objects/` and `fs-files/` directories are on the corresponding working-store
filesystems. Copy repositories have no same-filesystem requirement. An
explicitly configured repository is therefore not an optional cache hint: an
absent or malformed local store is a request error even when the goal is
already complete locally.

Remote setup loads pinned public keys and optional CA configuration without
fetching the repository. The first use authenticates the master and freezes
one snapshot shared by that backend's capabilities for the run. Build/reuse
indexes and object/fs-file lists are loaded lazily when needed. A complete
working-store hit can therefore avoid all remote requests, though invalid
local key or CA files still fail setup. See
[Store](./STORE.md#secondary-provider-capabilities).

## Graph planning

The request is validated and converted to a planned graph before realization
starts. Planning begins at `goals`; nodes that are not reachable from a goal
are not parsed as executable subjects and do not enter the scheduler.

The graph is keyed internally by `BuildKey`. Repeated recipe nodes with the
same `BuildKey` therefore denote one scheduled computation. Multiple goals may
share any part of the graph.

Each goal starts as an independent Tokio task. Goals may finish in any order,
but their results are collected by their original position in the request, so
the CLI response preserves request order.

## There is no global ready queue

The `DynamicRealizer` does not maintain a central queue containing every node
whose dependencies are ready. Instead, realizing a node creates asynchronous
work for its immediate inputs. Those inputs recursively do the same when their
content or output identity is actually needed.

This gives the scheduler two important properties:

- an exact hit can prune a dependency subtree before its inputs are visited;
- a reuse hit can use hash-only input identities without acquiring all input
  content.

It also means there is no scheduler-wide priority, critical-path policy, or
fairness order between independent DAG branches. Tokio decides which ready
tasks are polled; only the bounded resource executors described below impose
queues and limits.

## Two resolution operations

Every reachable node supports two logically different operations.

### Candidate resolution

`candidates(BuildKey)` returns an ordered, duplicate-free list of possible
output `ObjectHash` values. It does not require those objects to be present in
the working store.

Candidate resolution can be cheap when a mapping supplies a hash. On a
complete mapping and reuse miss it may recurse and eventually run a builder,
because a newly built output hash cannot be known in advance.

### Local realization

`realize_local(BuildKey)` returns one `ObjectHash` whose complete content is
available in the working store. Public goals always use this operation.
Builders also use it for their actual inputs once reuse resolution has shown
that a build may be necessary.

The separation is load-bearing. Candidate hashes are sufficient to compute
possible parent reuse keys. Local realization is deferred until content is
actually needed by a goal or a builder execution.

## Deduplication

The realizer maintains three asynchronous, per-run result tables:

| Key | Computation | Purpose |
|---|---|---|
| `BuildKey` | candidate resolution | compute one candidate list for every node identity |
| `BuildKey` | local realization | realize shared dependencies and duplicate goals once |
| `ObjectHash` | content lookup/import | prevent concurrent imports of the same known object |

Each table entry is a Tokio `OnceCell` shared by every waiter. The mutex around
each table protects only creation and lookup of cells; it is not held while the
underlying operation runs.

If local realization later produces a hash absent from a node's cached
candidate list, subsequent candidate requests append that hash to the result.

An unavailable known object is not cached permanently in the content table.
The failed-to-locate cell is removed, allowing a later Source materialization
or another completed operation to make the same hash available.

## Candidate resolution order

For one node, candidate resolution follows this order.

1. Read the working-store `builds/<build-key>` mapping. The mapping supplies a
   hash only; its content is not checked at this stage.
2. On a working mapping miss, query every configured mapping provider for build
   mappings. Candidate hashes retain provider priority and are deduplicated.
3. For a Source with no exact mapping, use its declared `object_hash`.
4. For a builder with no exact mapping, calculate possible reuse keys from its
   input candidate sets.
5. If no reuse mapping supplies a candidate, realize and run the builder to
   discover its output hash.

A working exact mapping short-circuits secondary exact lookup. If its hash
later proves unavailable from the working store and all configured content
sources, local realization falls back to the node's Source or builder path.
It does not return to secondary exact mappings to ask for a different hash
during candidate fallback. Publication may separately re-query mappings.

## Dynamic reuse resolution

Reuse resolution works with possible input identities before requiring input
content.

For a builder exact miss, the realizer starts candidate resolution for all
immediate inputs concurrently. Input results are restored to deterministic
input-name order after the tasks finish.

If an input has multiple candidate hashes, the realizer enumerates the
Cartesian product of all input candidate sets. Input slots are ordered by name,
candidate order is preserved within each slot, and the last slot changes
fastest. Every combination is a `name -> ObjectHash` map from which the builder
computes one `ReuseKey`.

The lookup order is:

1. inspect working-store reuse mappings for every candidate combination;
2. if none of them hit, query mapping providers for reuse mappings in batches
   of 256 keys;
3. collect ordered, duplicate-free output hash candidates;
4. if there is still no reuse candidate, proceed toward a real build.

Any working reuse candidates take priority as a group: secondary reuse mappings
are not queried during this candidate pass when at least one working mapping
was found.

The Cartesian product can grow exponentially in the number of conflicting
input candidates. In the normal case every input has one candidate and exactly
one reuse key is computed.

## Local realization

Local realization first obtains the node's candidate list. It then tries each
candidate hash in order.

For one known hash, content resolution does the following:

1. accept it if the object and any fs-tree closure are complete in the working
   store;
2. otherwise ask the configured content sources whether they can provide it;
3. import and verify obtainable content into the working store;
4. report a content miss if no source can provide a complete object.

Mapping lookup is not repeated during this content pass. The secondary
resolver's mapping and content capabilities are independent. Mapping and
availability queries run concurrently across providers, but their results are
merged in configuration order, not completion order. Imports try content
providers in that order. For an fs-tree, different providers may
provide disjoint missing fs-files and the manifest object, but the working
manifest is not published until its full closure is complete. Every actual
hardlink, copy, or download is verified and atomically published under the
working-store name; no result remains dependent on a provider directory entry
or continued remote access.

Remote fs-files are fetched concurrently in batches of at most 256 files.
After all representations in a batch arrive, one blocking namespace operation
verifies and publishes the batch under a local-I/O permit. The next batch then
starts. There is no namespace-runtime call per network response, and downloads
do not hold a local-I/O permit while waiting for the network.

After a builder candidate becomes local, the realizer publishes the current
build mapping and user-facing ref. It also records every reuse key known to
resolve to the selected builder output when that publication came through reuse
rather than exact identity. Source hits record Source metadata and refs, not
builder build/reuse mappings.

If all known candidates miss content, the node-specific fallback runs:

- a Source is materialized from its origin;
- a builder realizes its inputs and may eventually execute.

## Source scheduling

A Source recipe declares its expected `ObjectHash`, so candidate resolution
does not need to read, download, or unpack its origin. Source content is
requested only by `realize_local` after working and secondary content lookup
has failed.

This is lazy acquisition, not prefetching. Merely being reachable from a goal
does not place a Source in a download queue. An exact or reuse hit above that
Source can prune it without any origin access.

When an origin is required:

- `Http` and `OciRegistry` acquisition runs asynchronously and uses network
  connection permits;
- `Path` acquisition runs in Tokio's blocking pool and uses a local-source
  permit;
- hashing, verification, and publication happen before the Source completes;
- waiters for the same node share its local-realization computation.

Source acquisition does not consume a builder slot.

### Consequence of lazy Source acquisition

The current order can leave the network idle during a cold build. Consider:

```text
parent builder
|-- source tarball
`-- generated rootfs
    |-- OCI image
    `-- OciExtract
```

The tarball's hash is known immediately, but the generated rootfs hash is not.
The rootfs must first download its OCI image and run `OciExtract`. Only then
can the parent `ReuseKey` be computed. If the parent has a complete reuse miss,
the scheduler finally calls `realize_local` for the tarball and starts its
download.

This behavior avoids downloads that a later reuse hit would make unnecessary,
but it does not overlap all knowable network work with CPU and disk work. The
current scheduler has no speculative Source prefetch state and does not cancel
an optional download after a late reuse hit, because optional downloads are
never started.

## Builder execution path

When a builder must produce a local result, the realizer follows this path.

1. Start `realize_local` for every immediate input concurrently.
2. Restore the input results to deterministic input-name order.
3. Compute the actual `ReuseKey` from those realized hashes.
4. Check the working reuse mapping and require its content.
5. Check secondary reuse mappings and try their content candidates.
6. On a complete reuse miss, prepare filesystem paths for all builder inputs.
7. Submit a `BuilderJob` to the `BuildExecutor`.
8. Wait asynchronously for a staged output.
9. Import and publish the staged output outside the synchronous builder call.

The second reuse check is necessary because candidate resolution may have
considered several identities, while builder execution uses one concrete set
of locally realized inputs. It also closes races with content or mappings that
became available while the inputs were being realized.

Inputs whose names begin with `_` are materialized as filesystem roots.
Ordinary inputs are passed as object paths. Input preparation is blocking
filesystem work and runs in Tokio's blocking pool rather than on an async
runtime worker.

## BuildExecutor

The `BuildExecutor` owns the explicit FIFO builder-job queue. It bridges
asynchronous DAG realization to synchronous builder code; network and local-I/O
work instead waits on semaphores.

At run startup, `bobr` resolves `jobs` from the request or from the host's
available parallelism. It creates the executor with:

```text
active worker limit = jobs
submission channel capacity = 2 * jobs
accepted active + waiting limit = 3 * jobs
```

The dispatcher drains the submission channel into its FIFO queue. A separate
accepted-work semaphore bounds active plus queued jobs and applies backpressure
to asynchronous submitters; channel capacity alone is not the total queue bound.

The executor consists of:

- one dedicated dispatcher OS thread;
- a FIFO queue ordered by command arrival;
- at most `jobs` active builder worker threads;
- one OS worker thread for each active synchronous builder;
- asynchronous one-shot completion channels back to the realizer.

FIFO order is the order in which concurrent realizer tasks successfully submit
commands. It is not a promise of request, graph, or lexical node order.

A worker returns a staged output and notifies the dispatcher that execution
has finished. Final store import and publication run separately on Tokio's
blocking pool; the builder worker does not wait for publication before becoming
available for another job.

## Independent concurrency limits

The request controls several independent resources.

| Setting | Work bounded | Queue or mechanism |
|---|---|---|
| `jobs` | synchronous builders | `BuildExecutor` FIFO queue and worker limit |
| `limits.max_connections` | Source HTTP attempts, OCI pulls, and remote-repository metadata/content requests | shared global network semaphore |
| `limits.per_host_default` | network operations charged to one host | shared per-host semaphore |
| `limits.per_host.<host>` | network operations charged to a named host | per-host override semaphore |
| `limits.max_local_jobs` | local provider mapping/availability queries and imports, Source materialization/import, remote content publication, and builder input preparation | one shared local-I/O semaphore |

Local provider copy and verification work runs through Tokio's blocking pool
after acquiring the shared local-I/O permit. It therefore neither occupies a
Tokio worker nor a builder slot. Local mapping lookup also uses this shared
permit. Remote metadata fetches use network permits, not local-I/O permits.

Each remote reader additionally bounds blocking ordinary-object decoding with
its own semaphore, currently set to `limits.max_local_jobs`. These per-backend
decoder semaphores are not the shared local-I/O semaphore. Builder-output
publication and HTTP archive finalization run on the blocking pool without a
shared local-I/O permit. The setting is therefore not a universal limit on all
filesystem work.

Network permit acquisition always takes the per-host permit before the global
permit. Every network task uses that order, avoiding a lock-order deadlock.
Waiting for a permit is cancellation-aware and is not counted as a download
attempt or timeout.

For Source HTTP and repository requests, permits cover one attempt and are
released before retry backoff. An OCI Source instead holds one permit pair
charged to the registry host for its entire pull, including its internal
requests and retries. It does not acquire a new scheduler permit per blob.

## Ordering and nondeterminism

The scheduler preserves these orders:

- goal results follow request order;
- builder inputs follow their deterministic name order;
- candidate hashes preserve resolution priority;
- reuse combinations follow deterministic Cartesian-product order;
- mapping and content provider results follow configuration order;
- accepted builder jobs start in FIFO submission order, subject to available
  worker slots.

The following timing is intentionally unspecified:

- which independent goal or input task finishes first;
- which concurrent branch submits its builder first;
- the interleaving of network, local I/O, secondary lookup, and builder work;
- the order in which progress events from independent work become visible.

The scheduler's correctness depends on identities, dependency relationships,
and publication invariants, not on those completion timings.

## Publication

Only complete, verified content is published as a successful local result.
Already stored content is checked for completeness, not rehashed on every hit;
see [Store: complete content](./STORE.md#complete-content).
Publication may write:

- the object and any fs-tree files;
- user-facing object metadata;
- `builds/<build-key>` and applicable `reuses/<reuse-key>` mappings;
- `object-refs/<name>` and, when materialized, `fs-tree-refs/<name>`.

Mappings encode object identity; they are not used as proof that content is
currently complete. Content is checked separately when a result is required
locally.

Publication is idempotent and races between computations producing the same
content converge on the same content-addressed object.

## Failure and cancellation

Expected mapping/content misses are not run failures. An exhausted remote
transport failure allows provider fallback and disables that backend for the
rest of the run. Authentication failures and invalid repository metadata are
fatal. A corrupt advertised content representation can fall back to another
content provider; already verified fs-files may remain available for that
fallback. Local-store failures are not silently converted to provider misses.

An error from any goal fails the whole realization run. The realizer then:

1. cancels the shared run token;
2. cancels the shared network scheduler, covering both Source and repository
   acquisition;
3. aborts the remaining goal tasks;
4. shuts down the `BuildExecutor`.

BuildExecutor shutdown removes queued jobs without starting them and signals
cancellation to every active job. It then waits for all active worker threads
to finish before acknowledging shutdown. Cancellation of synchronous builder
code is cooperative: an already running builder or child process may take time
to reach a cancellation-aware boundary.

Network permit waits, retry backoffs, and transfers observe shared run
cancellation. Failed or cancelled acquisition keeps partial data in per-run
work only where the relevant origin implementation explicitly permits it;
partial content is never published as the declared object.

Waiting for the shared local-I/O permit is cancellation-aware. Once a
synchronous filesystem or namespace-runtime operation has started, it is not
forcibly interrupted: it runs to its atomic publication boundary and the
Realizer checks cancellation immediately afterwards. Such a completed
transaction may leave verified content and its content-level or Source
metadata as a reusable cache hit, but it does not let the cancelled realization
continue into dependent work. Repository staging guards remove unpublished
partial copies on failure; cancellation never exposes a half-copied object or
fs-file.

Progress reporting observes scheduler events but does not make scheduling
decisions. Terminal height, visible activity slots, and quiet mode affect only
presentation. See [Build logging](./LOGGING.md).

## Worked cases

### Working exact hit

The goal's build mapping supplies hash `X`. If `X` is complete in the working
store, the goal is published immediately. Its dependency subtree is never
visited.

### Hash-only reuse chain

A parent exact miss asks its inputs for candidates. Secondary mappings provide
hashes for those inputs without importing them. Their combination produces a
reuse key whose mapping supplies the parent's output hash. Only the final goal
content is required locally; intermediate input content remains absent.

### Cold builder miss

A builder has no exact or reuse mapping. Its immediate inputs are locally
realized in parallel. After the final reuse check misses, input paths are
prepared and the builder enters the FIFO executor. It starts when one of the
`jobs` worker slots is free, then its staged output is imported and published.

## Current invariants

The current scheduler maintains these central invariants:

- every public goal resolves to complete content in the working store;
- identity lookup and content acquisition remain separate;
- shared nodes and known content acquisition are deduplicated within a run;
- exact and reuse hits can prune unnecessary dependency content;
- synchronous builders never block Tokio runtime workers;
- builders, network transfers, and the shared class of local-I/O operations
  have explicit, independent bounds;
- builder inputs are complete before execution;
- staged output is published only after successful builder completion;
- one goal failure or external cancellation terminates the whole run;
- result correctness does not depend on the timing of independent tasks.
