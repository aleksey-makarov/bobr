# fs-tree Manifest

## Summary

This document is the normative definition of the `fs-tree manifest` format.

`fs-tree manifest` is the canonical manifest format for manifest-addressed
filesystem trees. It describes filesystem tree structure, directory metadata,
symlink metadata, and references to regular file payloads stored in
`<store>/fs-files/`.

The manifest itself is an ordinary store object, produced by builders such as
`FsTreeImport`, `OciExtract`, `TreeMerge`, `TreeSubset`, and `Sandbox`.
Materialized roots are cache entries under
`<store>/fs-trees/<manifest-object-hash>/`, not the payload stored in
`<store>/objects/`.

## Canonical JSONL Format

The manifest is UTF-8 JSON Lines. The first line is a mandatory schema header:

```json
{"schema":"bobr-fs-tree-manifest"}
```

Each following line is one filesystem entry:

```jsonl
{"schema":"bobr-fs-tree-manifest"}
{"p":"","t":"d","u":0,"g":0,"m":493}
{"p":"bin","t":"d","u":0,"g":0,"m":493}
{"p":"bin/tool","t":"f","h":"1111111111111111111111111111111111111111111111111111111111111111"}
{"p":"bin/tool-link","t":"l","u":0,"g":0,"x":"tool"}
```

Every line ends with `\n`, including the final line. Empty manifests, empty
lines, missing final newlines, extra whitespace, and non-canonical field order
are invalid.

## Entry Types

Directory entry:

```json
{"p":"usr/bin","t":"d","u":0,"g":0,"m":493}
```

- `p`: relative UTF-8 path.
- `t`: entry type, always `"d"`.
- `u`: logical uid, unsigned 32-bit integer.
- `g`: logical gid, unsigned 32-bit integer.
- `m`: mode, unsigned integer in `0..=0o7777`.

Regular file entry:

```json
{"p":"usr/bin/tool","t":"f","h":"<fs-file-hash>"}
```

- `p`: relative UTF-8 path.
- `t`: entry type, always `"f"`.
- `h`: opaque fs-file hash, encoded as exactly 64 lowercase hex digits.

Regular file bytes, uid, gid, and mode are not stored in the manifest. They are
part of the referenced fs-file object. The fs-file hash algorithm is not
defined by this document.

Symlink entry:

```json
{"p":"bin/sh","t":"l","u":0,"g":0,"x":"busybox"}
```

- `p`: relative UTF-8 path.
- `t`: entry type, always `"l"`.
- `u`: logical uid, unsigned 32-bit integer.
- `g`: logical gid, unsigned 32-bit integer.
- `x`: UTF-8 symlink target string.

Symlink entries do not carry an `h` field. Their target and ownership metadata
are stored directly in the manifest.

## Canonicalization Rules

The schema header must be byte-for-byte canonical:

```json
{"schema":"bobr-fs-tree-manifest"}
```

Filesystem entries are sorted by path bytes after UTF-8 encoding. The root
directory path is the empty string and therefore sorts first.

Field order is fixed:

- directory: `p`, `t`, `u`, `g`, `m`
- regular file: `p`, `t`, `h`
- symlink: `p`, `t`, `u`, `g`, `x`

Objects must contain exactly the fields required for their entry type. Unknown
fields, missing fields, duplicate fields, alternate key order, and added
whitespace are rejected.

Strings are written without optional JSON escapes. Only `"` and `\` are
escaped. Control characters are invalid in paths and symlink targets.

Together these rules give a tree exactly one valid byte encoding: the parser
accepts only that encoding and rejects any deviation in entry order, field set,
field order, whitespace, or escaping. Because the manifest is an ordinary store
object, its `ObjectHash` (see [Filesystem Object Hashing](./FSOBJ_HASH.md)) is
taken over these canonical bytes and is therefore a deterministic function of
the tree.

## Tree Shape Rules

The manifest must contain at least the root directory entry:

```json
{"p":"","t":"d","u":0,"g":0,"m":493}
```

Every non-root entry must have an explicit parent directory entry in the same
manifest. Duplicate paths are invalid. A file or symlink cannot be the parent
of another entry.

Paths are always relative UTF-8 strings:

- no leading `/`;
- no trailing `/`;
- no empty components;
- no `.` or `..` components;
- no control characters.

## Store Integration

Regular file entries reference opaque fs-file hashes. The file bytes and file
metadata represented by those hashes live in `<store>/fs-files/`; the manifest
does not inline them. Directory and symlink metadata are stored directly in the
manifest.

The runtime materializes a manifest object only when a builder input asks for a
filesystem root. The cache path is `<store>/fs-trees/<manifest-object-hash>/`.
For named materializations, the runtime also updates
`<store>/fs-tree-refs/<name>` to point at that cache root. These refs are
user-facing inspection aids; runtime cache lookup does not read them.
Builders that only need to read or transform manifests, such as `TreeMerge`
and `TreeSubset`, consume the manifest object directly and do not materialize
the tree.

## Scope Boundaries

The manifest format intentionally does not preserve original inode topology.
Materialization may hardlink equal fs-file objects to the same inode. `st_ino`
and `st_nlink` are not semantic fs-tree identity.

The first manifest implementation does not model xattrs, POSIX ACLs, or
file capabilities. Those must be added before claiming complete Linux rootfs
metadata preservation.
