# mbbin Design (MVP Agreements)

This document captures the current agreed contract for `mbbin`.

## Purpose

`mbbin` is a binary builder that runs builds inside short-lived containers.
It reads recipes from Nickel and writes declared outputs into shared materialized storage.

## Runtime Layout

`mbbin` uses shared builder root `./.mbuild/`:

- `recipes.ncl`: shared recipes file (for all builders).
- `materialized/`: shared materialized inputs and outputs.

## Recipe Model (MVP)

Each recipe declares:

- `inputs`: named input directories.
- `outputs`: named output directories.
- `script`: script text to execute in the container.

Important notes:

- Input/output names are used both as materialized directory names and mount-point names.
- Names are case-sensitive and must be validated (safe characters only).
- Container image is implicit/standard in current MVP (not declared in recipe yet).

## Container Execution Contract

- Build runs in a one-shot container (`podman run --rm`).
- No long-lived containers.
- Current MVP uses an implicit standard image (hardcoded by the builder), not recipe-configurable image.
- Network is disabled (`--network=none`) for hermeticity.
- Container runs as host caller UID/GID (`--user <uid>:<gid>`).
- Output file ownership must match host caller user.

## Mount Contract

- Inputs are mounted as overlay writable (`:O`) at `/in/<name>` (writes stay in container overlay layer and do not modify host input directories).
- Outputs are mounted writable as `/out/<name>`.
- Builder creates empty output directories before execution.
- Builder guarantees that every declared output directory exists (created by builder).
- Builder does not validate output contents beyond directory existence in MVP.
- Host output directories are `./.mbuild/materialized/<output-name>`.

## Script Semantics

- `script` is executed as provided by recipe text.
- Shebang line (`#!/usr/bin/env ...`) is expected in script text.
- Success/failure is determined only by script exit code.
- Missing interpreter is treated as a normal build failure.

## Error Policy (MVP)

- Non-zero container/script exit code => build failure.
- Missing interpreter or missing runtime tools in container image => build failure.
- Output semantic validation is deferred.

## Future Direction

- A separate container-image builder is expected to manage build images.
- After that, image references for `mbbin` will become explicit build inputs.
- This is part of stronger hermeticity across source, image, and binary builders.
