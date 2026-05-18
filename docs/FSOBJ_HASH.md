# Filesystem Object Hashing

This document defines the hashing rules for filesystem objects.

## Summary

Filesystem object hashing computes structural hashes for two input sources:

- a filesystem path, whose root may be a regular file or a directory
- a tar archive, which always describes a root directory

The invariant is:

> `hash_path(dir)` equals `hash_tar_reader(tar(dir))` whenever the tar archive
> represents the same normalized tree under the rules below.

## Supported Object Kinds

Supported root objects:

- regular file
- directory

Supported entries inside a directory or tar-described tree:

- regular file
- directory
- symlink

Rejected cases:

- root symlink
- device nodes
- fifo
- socket
- tar hardlinks
- any unsupported tar entry kind

## Hash Identity Rules

### Regular file

A regular file hash depends on:

- file bytes
- executable bit only

All other mode bits are ignored.

This is intentionally narrower than install semantics:

- install metadata controls the final installed `uid`, `gid`, and unix mode
- the payload executable bit is preserved as part of file identity
- rootfs composition uses that bit only to choose between
  `regular_file_mode` and `executable_file_mode`

### Symlink

A symlink hash depends on:

- raw symlink target bytes

The target is not resolved. Target existence is irrelevant.

### Directory

A directory hash depends on:

- child entry names
- child entry kinds
- child hashes

Child entries are sorted by raw name bytes.

## Leaf Indexes

`fsobj-hash` can compute a leaf index while hashing a filesystem path.
The index records only regular file and symlink node hashes, keyed by
object-relative path. Directory entries are intentionally not persisted in the
index.

Directory hashes are cheap to recompute from structure:

- regular file and symlink hashes come from the leaf index
- directory entries come from the caller's tree description
- empty directories are represented by hashing a directory with no children

This keeps the index as a cache for expensive payload-byte hashing, not as a
second persisted Merkle tree format.

Synthetic fs-tree objects use the same fsobj hash algorithm. Their object hash
is the directory hash of two entries:

- `manifest.jsonl`
- `root`

The manifest file node hash can be computed directly from canonical manifest
bytes. The `root` directory hash can be recomputed bottom-up from the
fs-tree manifest plus leaf hashes.

## Ignored Metadata

The hash ignores:

- absolute root path
- uid/gid
- uname/gname
- mtime/ctime/atime
- xattrs and ACLs
- directory mode
- symlink mode
- tar header layout
- tar entry order

## Tar Normalization Rules

Tar archives are normalized before hashing.

Allowed normalizations:

- `./foo` becomes `foo`
- repeated `/` are collapsed
- trailing `/` on directory entries is ignored

Rejected archive paths:

- absolute paths
- paths containing `..`
- paths that normalize to empty

Implicit parent directories are synthesized. For example, `a/b/c.txt` implies the
existence of `a/` and `a/b/` even if the archive does not list them explicitly.

Repeated explicit directory entries are allowed as no-op. Duplicate file or
symlink entries are rejected. Any path kind conflict is rejected.

## CLI

The `fsobj-hash` crate also provides a small helper binary:

```text
fsobj-hash <path> [--mode=auto|direct|tar]
```

It prints a single lowercase hex `object_hash` to stdout.

`fsobj-hash --help` prints the command usage and available modes.

### Modes

- `direct`
  - hash the given filesystem path with `hash_path`
- `tar`
  - hash the given tar archive with `hash_tar_file`
- `auto`
  - if `<path>` is a directory, use `hash_path`
  - if `<path>` is a regular file whose basename ends with `.tar`, use `hash_tar_file`
  - otherwise use `hash_path`

### Tar From Stdin

The CLI supports tar input from stdin only in explicit tar mode:

```text
fsobj-hash - --mode=tar
```

`-` is not accepted in `auto` or `direct` mode.

## Path Encoding Boundary

The generic `fsobj-hash` algorithm treats directory entry names and symlink
targets as raw Unix bytes. It can hash paths that are not valid UTF-8.

`mbuild` fs-tree objects are stricter: `manifest.jsonl` stores paths and
symlink targets as JSON strings, so fs-tree manifests are UTF-8-only. This is a
property of the mbuild fs-tree object format, not of the generic fsobj hash
algorithm.
