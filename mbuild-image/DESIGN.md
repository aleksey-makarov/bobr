# mbuild-image Design (MVP)

## Scope and Goals

`mbuild-image` is a builder that creates a new container image from:

- one base image artifact;
- zero or more binary artifacts.

This builder exists to make build environments explicit, reproducible, and hash-addressed in the same artifact model as other builders.

Primary MVP goal:

- prepare build environments (for example, LFS stage environments) by installing binary artifacts into an image root filesystem;
- produce one resulting image artifact that can be consumed by other builders (especially `mbuild-binary`).

## Non-Goals (MVP)

- no alternative install strategies (copy is the only method);
- no conflict resolution modes (any conflict is an error);
- no owner policy modes such as `preserve`/`root` (owners are taken only from artifact metadata rules);
- no orchestration features.

## Recipe Model

`mbuild-image` recipe type: `type = "image"`

Expected inputs:

- exactly one artifact with `artifact_kind = "container-image"` (base image);
- one or more artifacts with `artifact_kind = "binary-output"` (installed into image).

Output:

- one artifact with `artifact_kind = "container-image"` (result image).

Input order is semantically significant. The builder processes install artifacts in the same order
as listed in the recipe.

## Binary Artifact Install Metadata

Each `binary-output` artifact must provide install metadata in its meta record:

```nickel
install = {
  owners = [
    { path = "bin/**", uid = 0, gid = 0 },
    { path = "lib/**", uid = 0, gid = 0 },
    { path = "var/log/lastlog", uid = 0, gid = 4 },
    { path = "**", uid = 0, gid = 0 },
  ],
}
```

Notes:

- `path` is a glob-like pattern relative to artifact root.
- `uid`/`gid` are numeric and required.
- There is no `install.method`; copy is implicit and the only behavior in MVP.

### Matching Rules

For every copied file, owner rule matching is strict:

- `0` matches -> error;
- `>1` matches -> error;
- exactly `1` match -> apply that `uid:gid`.

No fallback, no implicit defaults.

## Conflict Policy

Path conflicts between installed artifacts are always fatal:

- if two artifacts map to the same target path in image rootfs, the build fails.

There is no overwrite mode in MVP.

## Image Creation Pipeline (Technical)

The image build is performed with exactly one container run and one commit for the entire recipe.

### Step 1: Resolve Inputs

1. Resolve all inputs from `.mbuild/refs`.
2. Detect base image artifact (`container-image`), validate exactly one.
3. Collect install artifacts (`binary-output`).
4. Read install metadata for each binary artifact and validate schema.

### Step 2: Build Installation Plan

1. Enumerate files of each binary artifact.
2. For each file:
   - compute destination absolute path under `/`;
   - resolve owner rule (must match exactly one pattern).
3. Check path collisions across all artifacts.
4. Produce deterministic install manifest:
   - sorted by destination path;
   - includes source file, destination path, uid, gid, mode.

### Step 3: Materialize and Install (Single Run)

1. Create a temporary container from base image.
2. Mount all binary artifact roots read-only (for example under `/mnt/artifacts/<name>`).
3. Mount generated install manifest read-only.
4. Execute installer script inside container:
   - create parent directories;
   - copy files into `/` preserving file mode bits;
   - apply ownership from manifest (`chown uid:gid`);
   - fail immediately on any collision or copy/chown error.

### Step 4: Commit and Publish

1. Commit container filesystem to a new local image.
2. Inspect resulting digest.
3. Publish output artifact:
   - object payload containing image reference/digest data;
   - metadata with provenance (base image digest + ordered installed artifact ids/hashes).
4. Remove temporary container and temporary tags.

## Bootstrap Mode (`from scratch`)

`mbuild-image` supports two execution modes:

- layered mode (base image is provided);
- bootstrap mode (no base image input).

If no `container-image` input is specified, the builder creates a base image from scratch.

In both modes, at least one `binary-output` input is required.

### Bootstrap Pipeline

1. Resolve `binary-output` inputs and their install metadata (`owners` rules).
2. Build a temporary rootfs directory on host:
   - copy files from all input artifacts;
   - apply strict owner matching (`0` matches -> error, `>1` matches -> error);
   - fail on any path conflict.
3. Pack temporary rootfs to `rootfs.tar`.
4. Create image with:
   - `podman import rootfs.tar <image-ref>`
5. Inspect resulting digest and publish output artifact metadata/provenance.
6. Remove temporary rootfs/tar files and temporary tags (best effort).

### Mode Selection Rule

- If recipe has one `container-image` input: layered mode is used (`create/run/commit`).
- If recipe has zero `container-image` inputs: bootstrap mode is used (`podman import` from generated rootfs tar).

## Determinism Requirements

To keep output stable:

- ordered inputs are taken from recipe order;
- ordered inputs are taken from recipe order;
- owner matching is strict and deterministic;
- installation plan is normalized and sorted;
- commit metadata should avoid non-deterministic values where possible.

## Provenance (Output Metadata)

Result image artifact metadata should include at least:

- base image artifact id and digest;
- ordered list of installed binary artifact ids and digests;
- builder identity (`mbuild-image`);
- timestamp fields (optional, if needed by global metadata policy).

## Error Classes (Planned)

- `invalid-recipe`
- `input-resolution-failed`
- `install-metadata-invalid`
- `owner-rule-no-match`
- `owner-rule-ambiguous`
- `path-conflict`
- `podman-failed`
- `publish-failed`

## Integration with mbuild-binary

`mbuild-binary` should eventually stop using a hardcoded standard image and consume a `container-image` input artifact explicitly.

This keeps environment definition in recipes and enables stage-specific build environments (for example LFS pass1/pass2/final).
