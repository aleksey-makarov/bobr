# Content-Addressed Store v1 for `mbuild`

## Summary

This design introduces a two-layer local store:

- `objects`: content-addressed payloads;
- `artifacts`: content-addressed semantic/provenance records that reference objects.

This is a deliberate split:

- object identity depends only on payload content;
- artifact identity depends on the full stable artifact record;
- human-facing names are not part of identity.

Locked decisions for v1:

- scope: local store only;
- internal runtime format: JSON;
- logical names, if present, belong to the Nickel package layer and are not part of CAS semantics;
- builders consume and produce artifacts, not refs;
- `artifact-refs` and `object-refs` are human-facing only and are not used by runtime semantics;
- no `-name-version` in canonical store identity;
- migration is a hard cutover;
- `container-image` objects are descriptor files in v1.
- materialize and roots are out of scope for this design.

## Store Layout

```text
.mbuild/
  objects/
    <object-hash>           # file or directory, this is the payload
  artifacts/
    <artifact-hash>.json    # artifact record, includes object-hash
  artifact-refs/
    <output-name>.json      # human-facing symlink to ../artifacts/<artifact-hash>.json
  object-refs/
    <output-name>           # human-facing symlink to object
  .. other builder-specific files and dirs ..
```

Notes:

- This section defines only the CAS namespaces and human-facing ref namespaces.
- It does not define the full `.mbuild/` layout.
- Each builder may own a same-named subdirectory under `.mbuild/` for builder-specific
  runtime state, temporary files, logs, and caches.
- `.mbuild/objects/<object-hash>` is the payload itself, not a wrapper directory.
- `.mbuild/artifacts/<artifact-hash>.json` is metadata only.
- refs are convenience views for a human or external scripts, not runtime state.
- old `.mbuild/meta` and old `.mbuild/refs` symlink layout are not part of the new design.

## Identity Model

### Object identity

`object-hash` is the hash of payload content only.

Object hashing rules:

- algorithm: `sha256`;
- object kind: `file` or `directory`;
- for files, the hash includes:
  - file bytes;
  - executable bit only;
- for directories, the hash is computed from a canonical recursive tree walk;
- tree walk order is strict lexicographic order by relative path;
- for each entry, the hash includes:
  - relative path;
  - entry kind: file, directory, symlink;
  - executable bit for regular files;
  - file content digest for files;
  - symlink target bytes for symlinks;
- the hash excludes:
  - uid, gid;
  - mtime, ctime;
  - xattrs;
  - inode/device data;
  - symlink mode;
  - artifact name or recipe name.

Consequences:

- identical payloads built in different temp directories get the same `object-hash`;
- the same payload may be referenced by many different artifacts;
- payload deduplication happens automatically.

### Artifact identity

`artifact-hash` is the hash of the canonical artifact JSON record.

`artifact-hash` includes:

- `artifact_kind`;
- `object-hash`;
- producer identity;
- ordered input artifact hashes;
- stable attrs;
- any other deterministic provenance fields intentionally defined by the artifact schema.

`artifact-hash` does not include:

- human refs;
- output names;
- temporary paths;
- timestamps;
- logs;
- host-specific incidental data.

Consequences:

- identical payload with different provenance produces different artifacts;
- the same object may be published under multiple artifacts;
- refs can be changed freely without changing identity.

## Object and Artifact Records

### Object

The object is the payload at:

```text
.mbuild/objects/<object-hash>
```

No separate object metadata file is required in v1.

If the runtime needs object kind, it derives it directly from the filesystem entry.

### Artifact record

Artifact records are stored at:

```text
.mbuild/artifacts/<artifact-hash>.json
```

Recommended shape:

```json
{
  "schema": "mbuild-artifact-v1",
  "artifact_hash": "sha256:...",
  "artifact_kind": "build-script|source-tree|fetched-file|binary-output|container-image|...",
  "object_hash": "sha256:...",
  "producer": {
    "builder": "text|fetch|binary|image|container-image|github",
    "recipe_type": "..."
  },
  "input_artifact_hashes": [
    "sha256:..."
  ],
  "attrs": {}
}
```

Rules:

- `input_artifact_hashes` are stored in recipe order;
- artifact identity depends on `input_artifact_hashes`, not on any logical names;
- `attrs` must contain only deterministic fields relevant to runtime or provenance;
- the canonical JSON form used for hashing must be stable and implementation-defined;
- if `artifact_hash` is stored in the record, it is not part of the hash input.

## Human-Facing Refs

### Artifact refs

Stored at:

```text
.mbuild/artifact-refs/<output-name>.json -> ../artifacts/<artifact-hash>.json
```

Purpose:

- easy navigation for humans;
- easy lookup from current package field names or selected output labels;
- optional compatibility surface for helper scripts.

These refs are strictly human-facing:

- they are not part of artifact identity;
- they are not used by recipe evaluation;
- they are not used by builder input resolution;
- they are not required for store correctness.

### Object refs

Stored at:

```text
.mbuild/object-refs/<output-name> -> ../objects/<object-hash>
```

Purpose:

- human-friendly direct access to payloads;
- convenient inspection of the object currently pointed to by an output name.

These refs are derived convenience links only:

- they are not required to evaluate recipes;
- builders do not read them;
- they do not define store reachability or identity.
- removing them must not affect store semantics.

## Runtime Model

### Selected term and dependencies

The CAS layer does not define name-based dependency semantics.

Resolution flow:

1. Rust receives one selected closed artifact term from the Nickel layer.
2. The interpreter recursively evaluates embedded dependency terms.
3. Dependency evaluation yields realized input artifacts.
4. Runtime passes those resolved artifact records into the builder.
5. When the builder publishes outputs, runtime creates new objects and artifacts.
6. Human-facing refs may then be updated as a convenience view, but they are not part of runtime semantics.

There is no persistent runtime namespace of names inside the store.

Builders never receive refs as their contract.

### Builder contract

Builders should operate on:

- artifact metadata for semantic input validation;
- object payload paths for actual file access;
- artifact records as the output publication unit.

Conceptually:

- builders consume artifacts;
- builders read payloads through the artifacts' `object_hash`;
- builders emit new payloads, which become objects;
- builders emit artifact metadata records that point to those objects.

## `container-image` Representation

In v1, a `container-image` object is stored as a file object whose contents are a
JSON descriptor, for example:

```json
{
  "schema": "mbuild-container-image-object-v1",
  "storage": "external-podman",
  "image_ref": "docker.io/...@sha256:...",
  "image_digest": "sha256:..."
}
```

Rules:

- the payload is still an object and gets an `object-hash` like any other file;
- the artifact record carries `artifact_kind = "container-image"` plus provenance;
- `binary` consumes the resolved artifact record and reads the object payload;
- a future OCI-in-store design may replace only the object payload representation.

## Builder-Specific Expectations

### General

- builders stop publishing directly to name-addressed object paths;
- builders stop reading ad-hoc `.ncl` metadata files;
- builder caches remain outside the immutable store;
- logs remain outside the immutable store.

### `text`

- output is usually a file object;
- executable mode for `build-script` must be part of object hashing;
- artifact attrs may include stable fields such as `source_bytes`.

### `fetch`

- downloaded blob cache remains in `.mbuild/fetch/cache`;
- unpacked or raw result becomes an object;
- artifact attrs should carry:
  - source URL that succeeded;
  - declared hash from recipe;
  - unpack flag;
  - archive format;
  - normalized root flag.

### `github`

- mirror state remains external in `.mbuild/github/mirrors`;
- exported checkout becomes a directory object;
- artifact attrs should carry:
  - owner;
  - repo;
  - rev.

### `binary`

- builder-specific runtime state lives under `.mbuild/binary/`;
- output staging remains in `.mbuild/binary/tmp`;
- each declared output directory becomes an object;
- artifact attrs should carry:
  - install ownership policy;
  - ordered input artifact hashes;
  - any stable install-related data needed by downstream image assembly;
- run logs stay only in `.mbuild/binary/logs`.

### `image` and `container-image`

- output payload is a file object containing the image descriptor in v1;
- artifact records carry semantic type and provenance;
- output names do not affect either object or artifact identity.

## CLI Implications

The intended CLI model is term-oriented.

Default behavior:

- `mbuild` reads `./.mbuild/recipe.ncl`;
- that file is expected to evaluate to one selected closed artifact term or bundle projection;
- the default action is to build that selected term.

Alternative entrypoint selection:

- the user may provide an alternative Nickel file on the command line;
- the selected file is expected to evaluate to one selected closed artifact term or bundle projection.

Additional commands may exist for:

- `info` and other introspection;
- builder-state inspection and management;
- debugging of the selected term or realized artifact.

`info` should show at least:

- current `artifact-hash`;
- referenced `object-hash`;
- `artifact_kind`;
- builder;
- input artifact count.

The exact CLI spelling is not fixed by this document.

## Cutover Rules

- new runtime reads and writes only the new CAS layout;
- old `.mbuild/meta` and old `.mbuild/refs` are ignored;
- old name-addressed payloads are ignored;
- no automatic migration is required;
- existing workspaces must rebuild artifacts into the new store;
- the default term entrypoint is expected at `.mbuild/recipe.ncl`, or at an explicit Nickel file path passed on the command line.

## Test Plan

### Object hashing

- identical files with identical relevant mode bits get the same `object-hash`;
- changing executable mode changes `object-hash`;
- identical directory trees imported from different temp paths get the same `object-hash`;
- changing file bytes changes `object-hash`;
- changing symlink target changes `object-hash`;
- changing uid/gid/mtime does not change `object-hash`.

### Artifact hashing

- same object with different producer or attrs yields different `artifact-hash`;
- same artifact record serialized twice yields the same `artifact-hash`;
- changing input order changes `artifact-hash`;
- changing only human refs does not change `artifact-hash`.

### Refs

- building an output updates the symlink `.mbuild/artifact-refs/<name>.json`;
- building an output updates the symlink `.mbuild/object-refs/<name>`;
- changing refs never mutates an existing object or artifact record;
- removing refs does not change store semantics;
- runtime resolves dependencies from the selected term structure, not from refs.

### Builder integration

- `text` preserves executable build scripts through object hashing;
- `fetch` deduplicates identical payloads under the same `object-hash`;
- `binary` consumes resolved artifacts and reads payloads through `object-hash`;
- `image` and `container-image` publish descriptor file objects and matching artifact records.

### Cutover

- new runtime ignores old `.mbuild/meta`;
- new runtime ignores old `.mbuild/refs`;
- a workspace with only old store data behaves as empty until rebuilt.

## Assumptions

- Unix hosts are the only supported environment for v1.
- `sha256` is the only hash algorithm used for both objects and artifacts in v1.
- refs are human-facing conveniences only.
- there is no persistent runtime name namespace in the store.
- CAS is driven by interpretation of one selected closed artifact term.
- full graph evaluation and derivation-style semantics are postponed.
- materialize and roots are out of scope for this document.
- descriptor-only `container-image` storage is a v1 implementation choice, not the long-term end state.
