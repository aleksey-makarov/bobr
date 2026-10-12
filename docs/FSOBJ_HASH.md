# Filesystem Object Hashing

This document defines the hashing rules for filesystem objects.

> **Scope.** These rules define how `bobr` computes the
> [`ObjectHash`](./CONCEPTS.md) of ordinary objects — regular files,
> directories, and tar archives. The algorithm deliberately ignores ownership
> (`uid`/`gid`) and every mode bit except the executable bit. Filesystem trees
> are different: they make ownership and mode part of file identity, so their
> files are content-addressed by a separate fs-file hash, described in
> [Filesystem trees](./FS_TREE.md) and the
> [fs-tree Manifest](./FS_TREE_MANIFEST.md). `ObjectHash` and `FsFileHash` are
> distinct semantic types and must not be interchanged; this does not imply
> that their digest values can never coincide.

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

A file is executable exactly when `(mode & 0o111) != 0`: any owner, group, or
other execute bit is set. This uses the filesystem mode for path input and the
regular-file header mode for tar input. All other mode bits are ignored.

A **non-executable** regular file hashes to *exactly* the SHA-256 of its bytes.
Its `object_hash` therefore equals the digest upstreams publish (crates.io and
`Cargo.lock`, `sha256sum`, release checksums), so a source can be pinned from a
published checksum without fetching and re-hashing it first.

An **executable** regular file uses a tagged hash instead (a SHA-256 over a
domain-separation tag, the executable bit, the size, and the SHA-256 of the
bytes). Executable and non-executable files with identical bytes therefore use
different hash inputs — the executable bit is part of file identity. The
asymmetry is deliberate: the "hash equals plain SHA-256" property holds only for
non-executable files, which is the common case for source archives and crates.
Directory and symlink hashes use their own tags, but these do not separate them
from arbitrary non-executable file bytes; see
[Cross-kind ambiguity](#cross-kind-ambiguity).

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

### Exact hash input encoding

Let `SHA256` return a raw 32-byte digest, `||` mean byte concatenation, and
`U64BE(n)` encode an unsigned integer as exactly eight bytes in big-endian
order. All lengths count bytes. Tags are literal ASCII bytes followed by one
NUL byte (`\0`), not the two characters backslash and zero:

```text
FILE_TAG    = b"fsobj:file:v1\0"
DIR_TAG     = b"fsobj:dir:v1\0"
SYMLINK_TAG = b"fsobj:symlink:v1\0"

non_executable_file(bytes) = SHA256(bytes)
executable_file(bytes)     = SHA256(FILE_TAG || 0x01 || U64BE(len(bytes))
                                   || SHA256(bytes))
symlink(target)           = SHA256(SYMLINK_TAG || U64BE(len(target)) || target)

entry(child) = kind(child) || U64BE(len(name(child))) || name(child) || hash(child)
directory(children) = SHA256(DIR_TAG || entry(child_1) || ... || entry(child_n))
```

The executable marker is one byte, `0x01`. Entry kinds are one ASCII byte:
`f` (`0x66`) for a file, `d` (`0x64`) for a directory, and `l` (`0x6c`) for a
symlink. A child name is a single raw path component, not its full relative
path. Children are ordered lexicographically by unsigned name bytes, with a
shorter prefix before a longer name. Child hashes are recursively computed
32-byte digests, not hexadecimal strings. There is no child count, separator,
padding, or terminator after the directory entries; an empty directory hashes
only `DIR_TAG`.

### Cross-kind ambiguity

The current encoding does not guarantee distinct identities across object
kinds. An empty directory and a non-executable file containing exactly the
13 bytes `b"fsobj:dir:v1\0"` have the same `ObjectHash`:

```text
b9fc118a1a95607accd6d739f6c0caa8658c415677c522ce48caa9d003f6e077
```

The file includes a final NUL byte and no newline. This is not a cryptographic
SHA-256 collision: both objects supply exactly the same bytes to SHA-256.
More generally, a non-executable file can contain the encoded hash input of a
tagged object. The tags separate the tagged encodings from each other, but not
from the unrestricted bytes of non-executable files. An `ObjectHash` alone
therefore does not uniquely identify an object kind; hash-keyed storage can
encounter different kinds with the same identity. This is a limitation of the
current algorithm, not a guarantee that such inputs are rejected.

## Leaf Hashes

The `fsobj-hash` library API `hash_path_with_leaf_index` returns an
`IndexedPathHash` containing the root `ObjectHash` and a transient `LeafIndex`.
The CLI does not expose this index; it prints only the root `ObjectHash`.
The index records only regular file and symlink node hashes, keyed by
object-relative path. Directory entries are intentionally not part of that
leaf set.

Current fs-tree manifest objects do not use this leaf index as their object
format. The manifest text is a normal store object payload. Regular file
payloads referenced by the manifest live under `<store>/fs-files/` and use
their own fs-file hashes.

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

These rules describe generic tar hashing through `fsobj-hash`'s
`hash_tar_reader` and `hash_tar_file` APIs and the CLI's `tar` mode. They are
not a transport-validation profile. Remote repository directory payloads must
also satisfy the stricter [repository tar profile](./REMOTE_REPOSITORY/TAR.md),
including its header, extension, padding, termination, and resource limits.
An archive accepted by generic hashing need not be valid repository content.

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

The `fsobj-hash` crate also provides a small `bobr-fsobj-hash` helper binary:

```text
bobr-fsobj-hash <path> [--mode=auto|direct|tar]
```

It prints a single lowercase hex `object_hash` to stdout.

`bobr-fsobj-hash --help` prints the command usage and available modes.

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
bobr-fsobj-hash - --mode=tar
```

`-` is not accepted in `auto` or `direct` mode.

## Path Encoding Boundary

The generic `fsobj-hash` algorithm treats directory entry names and symlink
targets as raw Unix bytes. It can hash paths that are not valid UTF-8.

`bobr` fs-tree manifest paths and symlink targets are UTF-8-only because
they are serialized as JSON strings. This is a property of the fs-tree manifest
format, not of the generic fsobj hash algorithm.
