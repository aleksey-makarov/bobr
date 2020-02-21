# Build System Design (Agreed Decisions)

This document captures only ideas that were explicitly proposed and accepted during discussion.  
It is a working snapshot of the current MVP understanding, without extensions that are intentionally postponed.

## 1. Project Overview

The system is intended for reproducible artifact builds and is conceptually close to Nix/Yocto, but with its own architecture.

Key characteristics:

- The system consists of multiple builder components.
- The first target builder set:
  - `source builder` (source retrieval).
  - `binary builder` (building binary outputs in containers).
- Recipes are described in Nickel.

## 2. Core Entities and Roles

In the currently agreed model:

- `Recipe` describes what should be done.
- `Builder` executes a recipe and produces outputs.
- `Output` has a type and a hash.
- `Materialization` is a separate operation from `build`.
- `Dematerialization` (removing a materialized output) is also a separate operation.

Important:

- Each builder has its own storage directory and its own materialized-output directory.
- A single global store is not part of the design.

## 3. Hashes and Identity

Agreed:

- Artifact identity is defined by its hash.
- Hashes are computed from output data:
  - If the output is a single file (for example, `tar`), hash the file.
  - If the output is a tree (`tree`), hash the tree.
- For `source builder`, output is always `tree`.

Consequence:

- Before `build` runs, a concrete output hash may be unknown.
- After `build`, output gets final identity through the computed hash.

## 4. Separation of `build` and `materialize`

This is a core requirement.

- `build`:
  - Performs builder work.
  - Produces a stored artifact in the builder's internal format.
  - Registers the mapping between recipe and produced outputs.
  - Computes artifact hash at this stage.
- `materialize`:
  - Takes an existing output.
  - Unpacks/expands it into the builder directory in a form usable by following steps.
- `dematerialize`:
  - Removes a previously materialized result on orchestrator request.

## 5. Source Builder (Current Fixed Model)

Agreed for the current stage:

- Source builder recipe has no inputs.
- At the first stage, only fixed versions are considered (not a function from package version).
- Target output:
  - Exactly the source tree for the `git commit/hash` defined in recipe.
- Source builder output:
  - Type: `tree`.
  - Hash: tree hash.

A separate output for "raw downloaded content" (for example, source archive as a separate entity) is not planned.

## 6. Binary Builder (Current Fixed Model)

Agreed:

- Binary builder performs builds in a container.
- Build result may be stored in builder-internal format (for example, a tar file).
- This result can then be materialized into binary builder directory.
- `build` and `materialize` are also separated for binary builder.

## 7. Passing Results Between Builders

Agreed direction:

- Builders should exchange a typed logical reference (`ref`), not a raw path.
- Materialized directory path is an internal builder implementation detail.

Reason:

- Logical refs isolate builders from each other.
- It reduces coupling to internal directory layouts across builders.

## 8. Materialized Outputs and Access Mode

Agreed:

- Materialized results must be `read-only`.

Why this matters:

- Reproducibility.
- Protection against accidental input mutation in later stages.

## 9. Orchestrator and Lifecycle Ownership

Agreed:

- There should be an orchestrator (pipeline control subsystem).
- Orchestrator asks builders to:
  - run `build`,
  - run `materialize`,
  - run `dematerialize` when needed.
- Builder tracks its own state:
  - what is already built,
  - what is already materialized.
- For now, we do not think about orchestrator at all and call operations manually.

## 10. Build Environment

Agreed key requirement:

- Build environment must be hermetic.
- Environment is treated as a separate builder output and therefore has its own hash.

Practical meaning:

- Build must depend on a hashable/addressable environment, not on random host state.

## 11. Intentionally Postponed

At this stage we intentionally do not add complexity in:

- Parallel builds and related atomicity mechanisms.
- Garbage collection (GC) policies and detailed cleanup strategy.
- Advanced schema versioning and multiple advanced output variants.
- Full provenance detail.

This does not reduce their importance; it only fixes the priority: first get a simple working flow.

## 12. Current Practical MVP Shape

Conceptual MVP currently looks like this:

1. There is a Nickel recipe.
2. Builder runs `build` and gets output(s) with hashes.
3. Builder can run `materialize` for a specific output.
4. Next builder uses a typed reference to the previous output.
5. Materialized data is available in read-only mode.
6. Orchestrator controls call order and `materialize/dematerialize` lifecycle.

This is the currently agreed baseline architecture line.
