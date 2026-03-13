# Content-Addressed Store v1 for `mbuild`

## Summary

This design introduces a local content-addressed store with one identity-bearing
entity:

- `objects`: content-addressed payloads.

Everything else is metadata:

- `meta`: technical metadata attached to an object;
- `meta-refs`: publication metadata attached to a published name;
- `object-refs`: human-facing symlinks to payload objects.

The key rule is:

- object identity depends only on payload content.

Names, provenance, builder attrs, and refs do not participate in identity.

Locked decisions for v1:

- scope: local store only;
- internal runtime format for metadata: JSON;
- object identity is only `object-hash`;
- technical metadata is keyed by `object-hash`, but is not identity-bearing;
- publication metadata is keyed by published name, but is not identity-bearing;
- builders consume objects and metadata, not refs;
- `meta-refs` and `object-refs` are human-facing publication state and are not used by builder dependency resolution;
- no `-name-version` in canonical store identity;
- migration is a hard cutover;
- `container-image` objects are descriptor files in v1;
- materialize and roots are out of scope for this design.

## Store Layout

```text
.mbuild/
  objects/
    <object-hash>         # file or directory, this is the payload
  meta/
    <object-hash>.json    # technical metadata for one object
  meta-refs/
    <name>.json           # publication metadata for one published name
  object-refs/
    <name>                # human-facing symlink to ../objects/<object-hash>
  .. other builder-specific files and dirs ..
```

Notes:

- This section defines only the CAS namespace and the publication/ref namespaces.
- It does not define the full `.mbuild/` layout.
- Each builder may own a same-named subdirectory under `.mbuild/` for builder-specific
  runtime state, temporary files, logs, and caches.
- `.mbuild/objects/<object-hash>` is the payload itself, not a wrapper directory.
- `.mbuild/meta/<object-hash>.json` is metadata about one object, not a second store entity.
- `.mbuild/meta-refs/<name>.json` is metadata about one publication of an object.
- refs are convenience views for a human or external scripts, not runtime dependency state.
- old `.mbuild/artifacts`, old `.mbuild/meta`, and old `.mbuild/refs` layouts are not part of the new design.

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
  - recipe name;
  - publication name;
  - provenance metadata;
  - builder attrs.

Consequences:

- identical payloads built in different temp directories get the same `object-hash`;
- the same payload published under different names remains the same object;
- identical payload with different provenance is still the same object;
- payload deduplication happens automatically.

## Metadata Records

### Object metadata

Object metadata is stored at:

```text
.mbuild/meta/<object-hash>.json
```

Recommended shape:

```json
{
  "schema": "mbuild-meta-v1",
  "object_hash": "sha256:...",
  "object_kind": "file|directory",
  "kind": "build-script|source-tree|fetched-file|binary-output|container-image|...",
  "producer": {
    "builder": "text|fetch|binary|image|container-image|github"
  },
  "input_object_hashes": [
    "sha256:..."
  ],
  "attrs": {}
}
```

Rules:

- the file is keyed by `object-hash`, not by its own hash;
- it may contain technical metadata and provenance;
- it must not contain publication names or aliases;
- it must not define identity;
- it may be rewritten if the implementation later decides to normalize or enrich metadata for the same object.

### Publication metadata

Publication metadata is stored at:

```text
.mbuild/meta-refs/<name>.json
```

Recommended shape:

```json
{
  "schema": "mbuild-publication-v1",
  "name": "buildscript-coreutils",
  "object_hash": "sha256:...",
  "meta": {
    "description": "...",
    "aliases": []
  }
}
```

Rules:

- publication metadata is keyed by published name;
- it may contain any human-facing fields derived from the Nickel request;
- it must point at one `object_hash`;
- it is mutable publication state, not a CAS entity;
- multiple publication records may point at the same object.

### Object refs

Stored at:

```text
.mbuild/object-refs/<name> -> ../objects/<object-hash>
```

Purpose:

- human-friendly direct access to payloads;
- convenient inspection of the object currently published under a name.

These refs are human-facing only:

- they are not part of object identity;
- builders do not read them;
- removing them must not affect store semantics.

## Runtime Model

### Selected request and dependencies

The store layer does not define name-based dependency semantics.

Resolution flow:

1. Rust receives one selected evaluated build request from the Nickel layer.
2. That request contains:
   - publication metadata in `meta`;
   - one selected closed build term in `build`.
3. The interpreter recursively evaluates embedded dependency terms.
4. Dependency evaluation yields realized input objects plus their technical metadata.
5. Runtime passes those resolved inputs into the builder.
6. When the builder publishes an output, runtime creates:
   - a new object in `objects/`;
   - technical metadata in `meta/`;
   - publication records and refs from the request metadata.

There is no persistent runtime namespace of names inside the store.

Builders never receive refs as their contract.

### Builder contract

Builders should operate on:

- object metadata for semantic input validation;
- object payload paths for actual file access;
- object metadata as the output publication unit.

Conceptually:

- builders consume objects;
- builders read payloads through their `object-hash`;
- builders emit new payloads, which become objects;
- builders emit technical metadata records that point to those objects;
- runtime handles publication metadata and refs.

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
- `meta/<object-hash>.json` carries the semantic type and provenance;
- `binary` consumes the resolved object plus its metadata;
- a future OCI-in-store design may replace only the object payload representation.

## Builder-Specific Expectations

### General

- builders stop publishing directly to name-addressed payload paths;
- builders stop reading ad-hoc `.ncl` metadata files;
- builder caches remain outside the immutable object store;
- logs remain outside the immutable object store.

### `text`

- output is usually a file object;
- executable mode for `build-script` must be part of object hashing;
- object metadata attrs may include stable fields such as `source_bytes`.

### `fetch`

- downloaded blob cache remains in `.mbuild/fetch/cache`;
- unpacked or raw result becomes an object;
- object metadata attrs should carry:
  - source URL that succeeded;
  - declared hash from recipe;
  - unpack flag;
  - archive format;
  - normalized root flag.

### `github`

- mirror state remains external in `.mbuild/github/mirrors`;
- exported checkout becomes a directory object;
- object metadata attrs should carry:
  - owner;
  - repo;
  - rev.

### `binary`

- builder-specific runtime state lives under `.mbuild/binary/`;
- output staging remains in `.mbuild/binary/tmp`;
- each declared output directory becomes an object;
- object metadata attrs should carry:
  - install ownership policy;
  - ordered input object hashes;
  - any stable install-related data needed by downstream image assembly;
- run logs stay only in `.mbuild/binary/logs`.

### `image` and `container-image`

- output payload is a file object containing the image descriptor in v1;
- object metadata carries semantic type and provenance;
- output names do not affect object identity.

## CLI Implications

The intended CLI model is term-oriented.

Default behavior:

- `mbuild` reads `./.mbuild/recipe.ncl`;
- that file is expected to evaluate to one selected request with fields:
  - `meta`
  - `build`
- the default action is to build the selected request's `build` term and then publish its `meta`.

Alternative entrypoint selection:

- the user may provide an alternative Nickel file on the command line;
- the selected file is expected to evaluate to one request of the same shape.

Additional commands may exist for:

- `info` and other introspection;
- builder-state inspection and management;
- debugging of the selected term or realized object.

`info` should show at least:

- current `object-hash`;
- object kind;
- builder;
- input object count;
- published names that currently point at this object, if available.

The exact CLI spelling is not fixed by this document.

## Cutover Rules

- new runtime reads and writes only the new object/meta/publication layout;
- old `.mbuild/artifacts`, old `.mbuild/meta`, and old `.mbuild/refs` are ignored;
- old name-addressed payloads are ignored;
- no automatic migration is required;
- existing workspaces must rebuild objects into the new store;
- the default term entrypoint is expected at `.mbuild/recipe.ncl`, or at an explicit Nickel file path passed on the command line.

## Test Plan

### Object hashing

- identical files with identical relevant mode bits get the same `object-hash`;
- changing executable mode changes `object-hash`;
- identical directory trees imported from different temp paths get the same `object-hash`;
- changing file bytes changes `object-hash`;
- changing symlink target changes `object-hash`;
- changing uid/gid/mtime does not change `object-hash`.

### Object metadata

- the same object may have technical metadata written at `.mbuild/meta/<object-hash>.json`;
- changing metadata does not change `object-hash`;
- technical metadata does not contain publication names.

### Publication metadata and refs

- building a named request updates `.mbuild/meta-refs/<name>.json`;
- building a named request updates `.mbuild/object-refs/<name>`;
- multiple names may point at the same `object-hash`;
- changing publication metadata does not change `object-hash`;
- removing refs does not change store semantics;
- runtime resolves dependencies from the selected term structure, not from refs.

### Builder integration

- `text` preserves executable build scripts through object hashing;
- `fetch` deduplicates identical payloads under the same `object-hash`;
- `binary` consumes resolved objects and reads payloads through `object-hash`;
- `image` and `container-image` publish descriptor file objects and matching object metadata.

### Cutover

- new runtime ignores old `.mbuild/artifacts`;
- new runtime ignores old `.mbuild/meta`;
- new runtime ignores old `.mbuild/refs`;
- a workspace with only old store data behaves as empty until rebuilt.

## Assumptions

- Unix hosts are the only supported environment for v1.
- `sha256` is the only hash algorithm used for object identity in v1.
- refs are publication conveniences only.
- there is no persistent runtime name namespace in the store.
- the store is driven by interpretation of one selected request with `meta` and `build`.
- full graph evaluation and derivation-style semantics are postponed.
- materialize and roots are out of scope for this document.
- descriptor-only `container-image` storage is a v1 implementation choice, not the long-term end state.
