# Artifact Dependency Design

This document defines only metadata-level dependencies between built artifacts.

## 1) `container-image` -> `container-image` (parent chain)

For artifacts with `artifact_kind = "container-image"`:

- `image_parent: null | artifact_id`

Semantics:

- `null` means a base image artifact.
- non-null means this image is a layer over another image artifact.

Shape:

- at most one parent per image artifact;
- linear parent chain.

Usage:

- when an image artifact is used, its full parent chain is required.

## 2) `binary-output` -> `binary-output` (runtime DAG)

For artifacts with `artifact_kind = "binary-output"`:

- `runtime_deps: Array artifact_id`

Semantics:

- this artifact requires those runtime artifacts to be present in the image.

Shape:

- zero to many dependencies per artifact;
- directed acyclic graph.

Usage:

- when building an image from binary artifacts, runtime dependency closure is included.
