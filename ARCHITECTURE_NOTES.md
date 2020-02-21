# Architecture Notes (Proposals and Implementation View)

This document contains engineering proposals for turning the current idea into a robust architecture.
It is not a list of mandatory decisions, but a recommended trajectory.

## 1. Architecture Goal

Build a system where:

- builders are independent and loosely coupled;
- output identity is content-based;
- orchestrator controls process, not builder internals;
- functionality can be expanded incrementally without large migrations.

## 2. Recommended Layer Model

### 2.1 Intent Layer

Describes requested work:

- which recipe to execute;
- in which builder;
- with which input refs;
- with which environment.

It is useful to compute `request_id` as a hash of normalized intent.

### 2.2 Realization Layer

Describes actual execution result:

- which output refs were actually produced;
- which execution metadata exists;
- where and when this happened.

Why split these:

- allows comparing "what was requested" vs "what was produced";
- allows caching by intent while trusting identity by content.

## 3. Minimal Inter-Builder Contract (Recommendation)

### 3.1 Typed Output Reference

Recommended format:

```json
{
  "builder": "source",
  "kind": "tree",
  "hash_algo": "sha256",
  "hash": "abcd...",
  "format": "raw-tree"
}
```

Fields:

- `builder`: owner/provenance of artifact.
- `kind`: `tree | file`.
- `format`: storage serialization (`raw-tree`, `tar`, `gzip-tar`, ...).
- `hash_*`: identity.

### 3.2 Builder API Calls

Minimum:

- `build(recipe, inputs, environment_ref) -> outputs`
- `materialize(output_ref, target) -> path`
- `dematerialize(target | materialization_id) -> ok`

Optional:

- `inspect(output_ref) -> metadata`

## 4. Recommended Builder Directory Layout

For each builder:

```text
<builder-root>/
  artifacts/
    <hash>                # internal format of build result
  materialized/
    <name-or-id>/         # read-only expansion
  index/
    recipes/
      <recipe_hash>.json
    requests/
      <request_hash>.json
    outputs/
      <output_hash>.json
```

Why `index/`:

- fast lookup without filesystem scanning;
- easier consistency checks;
- simple base for future GC.

## 5. Recipe Normalization

Strong recommendation:

- normalize recipe before hashing (canonical field order, removal of insignificant defaults, canonical JSON/Nickel representation).

Why:

- otherwise same meaning can produce different hash ids;
- caching becomes unstable.

## 6. Tree Hashing (Unification Recommendation)

One fixed algorithm is needed for `tree`:

- lexicographic path sorting;
- node type (`file/dir/symlink`) included;
- permissions included;
- file content included;
- unstable attributes (mtime, uid/gid) excluded unless explicitly required.

This is key for cross-machine reproducibility.

## 7. Environment as an Artifact

The accepted idea is correct: environment is a separate hashed output.
Recommended extension:

- `binary build` accepts `environment_ref`;
- `request_id` includes `environment_ref`;
- changing environment always changes request identity.

## 8. Source of Truth for Materialize

Important: do not bind system to paths directly.

Recommendation:

- orchestrator stores `output_ref`;
- materialized path is treated as cache/view;
- materialized copy can be removed and restored from `artifacts/`.

This keeps behavior independent from current `materialized/` directory state.

## 9. Simple Rollout Path (By Stages)

### Stage 1: Working MVP

- `source builder` with fixed git commit.
- `binary builder` with one output kind (for example tar).
- `build/materialize/dematerialize` operations.
- read-only materialized directories.

### Stage 2: Stability

- canonical recipe hash;
- canonical tree hash;
- minimal indexes in `index/`;
- materialize restoration from artifacts.

### Stage 3: Operability

- explicit lifecycle and garbage collection;
- extended provenance;
- baseline error and retry policies.

## 10. Provenance (Recommended Minimum, When Ready)

Minimal useful set:

- `request_id`
- `recipe_hash`
- `builder_name`, `builder_version`
- `input_refs`
- `environment_ref`
- `output_refs`
- `started_at`, `finished_at`
- `exit_status`

Even this minimum significantly simplifies mismatch debugging.

## 11. Main Risks If Delayed Too Long

1. No canonical tree hash

- risk: false cache misses and non-reproducible results across hosts.

2. No mapping indexes

- risk: slow lookup and difficult recovery of recipe-output relations.

3. Passing raw paths between builders

- risk: fragile integration and dependency on internal layout.

4. Implicit environment dependency

- risk: same recipe yields different outputs without clear explanation.

## 12. Practical Compromise Principle

Pragmatic course:

- keep MVP truly minimal;
- but lock down formal-stability points early (`ref`, hash, output types, `build/materialize` separation);
- add everything else iteratively.

This allows fast progress without creating architecture debt that is hard to repay later.
