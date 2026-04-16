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
