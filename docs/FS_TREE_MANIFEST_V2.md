# fs-tree Manifest v2

## Summary

`fs-tree manifest v2` is the canonical manifest format for the future
manifest-addressed fs-tree subsystem. It describes filesystem tree structure,
directory metadata, symlink metadata, and references to regular file objects.

This document defines the manifest format only. It does not define the
`fs-files` store layout, fs-tree materialization cache, scanner, materializer,
or builder migration path.

The current production fs-tree object format remains unchanged until builders
are migrated explicitly.

## Canonical JSONL Format

The manifest is UTF-8 JSON Lines. The first line is a mandatory schema header:

```json
{"schema":"mbuild-fs-tree-manifest-v2"}
```

Each following line is one filesystem entry:

```jsonl
{"schema":"mbuild-fs-tree-manifest-v2"}
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
- `h`: opaque future fs-file object hash, encoded as exactly 64 lowercase hex
  digits.

Regular file bytes, uid, gid, and mode are not stored in manifest v2. They are
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
{"schema":"mbuild-fs-tree-manifest-v2"}
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

## Scope Boundaries

Manifest v2 intentionally does not preserve original inode topology. Future
materialization may hardlink equal fs-file objects to the same inode. `st_ino`
and `st_nlink` are not semantic fs-tree identity.

The first manifest v2 implementation does not model xattrs, POSIX ACLs, or
file capabilities. Those must be added before claiming complete Linux rootfs
metadata preservation.

The initial `mbuild-fs-tree` crate implements parser, writer, validation, and
the typed `FsFileHash` string reference. It does not implement fs-file storage,
fs-file hashing, scanner, materializer, or builder integration.
