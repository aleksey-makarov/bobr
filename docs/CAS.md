# Content-Addressed Store

## Summary

`mbuild` stores realized build results as content-addressed objects and stores
builder invocations as persistent build records.

- `objects/` holds payloads addressed by `object_hash`.
- `builds/` holds build records addressed by `build_key`.
- `meta-refs/` holds symlinks from published name to build record.
- `object-refs/` holds symlinks from published name to payload object.

Object identity depends only on payload content. Names, provenance, builder attrs,
and refs do not participate in object identity.

## Layout

```text
.mbuild/
  objects/
    <object-hash>
  builds/
    <build_key>.json
  meta-refs/
    <name>.json -> ../builds/<build_key>.json
  object-refs/
    <name> -> ../objects/<object-hash>
  .. builder-specific files and dirs ..
```

`objects/<object-hash>` is the payload itself, either a file or a directory.

`builds/<build_key>.json` is the persistent record of one interpreted builder
invocation.

`meta-refs/<name>.json` is a human-facing symlink to a build record.

`object-refs/<name>` is a human-facing symlink to the payload object.

Each builder owns a same-named subdirectory under `.mbuild/` for builder-specific
runtime state, temporary files, logs, and caches.

## Object Identity

`object-hash` is the hash of payload content only.

Hashing rules:

- algorithm: `sha256`
- root object kind: `file` or `directory`
- for regular files, the hash includes:
  - file bytes
  - executable bit only
- for directories, the hash includes:
  - relative paths
  - entry kinds: file, directory, symlink
  - executable bit for regular files
  - file content digest for files
  - symlink target bytes for symlinks
- directory traversal order is strict lexicographic order by relative path

The hash excludes:

- uid, gid
- mtime, ctime, atime
- xattrs and ACLs
- inode or device data
- symlink mode
- recipe names
- publication names
- provenance metadata
- builder attrs

Consequences:

- identical payloads built in different temp directories have the same `object-hash`
- identical payloads produced by different builders are the same object
- one object may be published under many names

## Build Records

Build records are stored at:

```text
.mbuild/builds/<build_key>.json
```

A build record describes one interpreted builder invocation and the object it
produced.

Example shape:

```json
{
  "schema": "mbuild-build-v1",
  "object_hash": "sha256:...",
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

- each build record is keyed by `build_key`
- a build record contains the metadata needed for further interpretation
- a build record points at exactly one `object_hash`
- multiple build records may point at the same object
- the same object may appear in build records with different builders or provenance

## Publication Refs

### Metadata refs

Stored at:

```text
.mbuild/meta-refs/<name>.json -> ../builds/<build_key>.json
```

Purpose:

- human-facing lookup from published name to build record
- access to builder metadata for the published name
- publication of one selected name or alias

These refs are publication state:

- they are not part of object identity
- builders do not read them directly
- removing them does not invalidate objects or build records

### Object refs

Stored at:

```text
.mbuild/object-refs/<name> -> ../objects/<object-hash>
```

Purpose:

- human-friendly direct access to payloads
- convenient inspection of the object currently published under a name

These refs are human-facing only:

- they are not part of object identity
- builders do not read them directly
- removing them must not affect store semantics

## Runtime Responsibilities

The store does not define dependency semantics. Dependency resolution comes from
the build term structure.

The runtime receives one evaluated build request with fields:

- `meta`: publication metadata requested by Nickel
- `build`: one closed build term

The runtime then:

1. recursively evaluates dependency terms inside `build`
2. resolves input objects through their build records
3. invokes the appropriate builder
4. stores the produced payload in `objects/`
5. writes one build record in `builds/`
6. updates one symlink in `meta-refs/`
7. updates one symlink in `object-refs/`

Builders do not read refs. Builders do not receive publication names as part of
build semantics.

## Builder Data Model

Builders consume:

- input object hashes
- input payload paths
- input build records for semantic validation

Builders produce:

- a payload that becomes one object
- a build record describing the invocation result

The runtime writes the build record and updates both publication ref namespaces.

## Container Image Objects

A `container-image` object is a file object whose contents are a JSON descriptor,
for example:

```json
{
  "schema": "mbuild-container-image-object-v1",
  "storage": "external-podman",
  "image_ref": "docker.io/...@sha256:...",
  "image_digest": "sha256:..."
}
```

The descriptor file is hashed like any other file object. The corresponding build
record carries the semantic type and provenance for that object.

## Builder-Specific Conventions

### `text`

- output is usually a file object
- executable mode for `build-script` participates in object hashing
- build attrs may include fields such as `source_bytes`

### `fetch`

- downloaded blob cache lives in `.mbuild/fetch/cache`
- unpacked or raw result becomes an object
- build attrs carry source URL, declared hash, unpack flag, archive format,
  and normalized-root information

### `github`

- mirror state lives in `.mbuild/github/mirrors`
- exported checkout becomes a directory object
- build attrs carry owner, repo, and rev

### `binary`

- runtime state lives in `.mbuild/binary/`
- output staging lives in `.mbuild/binary/tmp`
- each declared output directory becomes an object
- build attrs carry install policy, ordered input object hashes, and
  stable install-related data needed by downstream image assembly
- run logs live in `.mbuild/binary/logs`

### `image` and `container-image`

- output payload is a file object containing the image descriptor
- build metadata carries semantic type and provenance
- output names do not affect object identity

## CLI View

`mbuild` reads one evaluated build request from `./.mbuild/recipe.ncl` by
default. Another Nickel file may be passed explicitly.

The selected request has fields:

- `meta`
- `build`

The default action is to build `build`, create a build record, and publish `meta`
through `meta-refs` and `object-refs`.

`info` shows at least:

- current `object-hash`
- current `build_key`
- object kind
- builder
- input object count
- the current publication name and selected metadata
