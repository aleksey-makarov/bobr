# Remote repository directory tar profile

Status: format design draft.

This document specifies the tar profile used for directory payloads in Bobr
remote repository objects. It is part of repository format version 1 and is
referenced by [`OBJECT.md`](OBJECT.md).

The profile applies only to decoded directory payloads under
`o/<ObjectHash>`. It does not restrict tar archives stored as ordinary file
objects, upstream source archives, or the generic archive input accepted by
`fsobj-hash`.

## Purpose

Tar is a family of related formats rather than one unambiguous wire format.
Different readers may otherwise disagree about PAX overrides, GNU extensions,
sparse files, path bytes, or archive termination. A repository reader must not
hash one interpretation of an archive and materialize another.

Repository format version 1 therefore uses a deliberately small profile. The
publisher controls this transport representation and does not need to preserve
the original archive representation of an object.

## Base format

Logical entries use POSIX ustar headers. The standard ustar `name` and `prefix`
fields jointly represent an entry path when it fits. The profile additionally
supports the GNU LongName and GNU LongLink extension entries described below.

The following tar extensions are not part of this profile and must be
rejected:

- local and global PAX extended headers;
- GNU and PAX sparse-file representations;
- old GNU multi-volume and continuation entries;
- all other vendor-specific or unknown extension entry types.

The restriction is intentional. PAX string semantics do not provide the raw
Unix-byte model required by `ObjectHash`, while GNU LongName and LongLink are
sufficient to represent long raw paths and symbolic-link targets.

## Logical entry types

Only these logical tar entry types are accepted:

- regular file, encoded with type flag NUL or `0`;
- directory, encoded with type flag `5`;
- symbolic link, encoded with type flag `2`.

Hard links, block and character devices, FIFOs, sockets, sparse files, and all
other logical entry types are rejected.

A regular-file entry contains exactly the number of bytes declared by its
effective size. Directory and symbolic-link entries have size zero and no data
payload. A symbolic-link target must be non-empty.

## GNU long-name extensions

A path that cannot be represented by the standard ustar `name` and `prefix`
fields is represented by one GNU LongName entry:

- its type flag is `L`;
- its header path is `././@LongLink`;
- its data is the complete raw path followed by exactly one NUL byte;
- its declared size includes that final NUL;
- it applies only to the immediately following logical entry, except that one
  GNU LongLink entry may occur between them.

A symbolic-link target that cannot be represented by the ustar `linkname`
field is represented analogously by one GNU LongLink entry with type flag `K`.
It applies only to the immediately following symbolic-link entry.

When both extensions are needed, the publisher emits LongName first, LongLink
second, and the logical symbolic-link entry last. No entry may have more than
one extension of either kind. An extension without a following applicable
logical entry is invalid.

The extension data contains no embedded NUL. After applying an extension, the
corresponding field of the logical entry is ignored for object semantics. A
publisher uses an extension only when the value does not fit in the standard
ustar fields.

GNU long-name entries are transport metadata. They do not themselves create
nodes in the normalized object tree.

## Path and string model

Entry paths and symbolic-link targets are raw Unix byte strings. They are not
decoded as UTF-8 and undergo no Unicode normalization. NUL cannot occur in
either value.

An effective path after applying any GNU LongName extension is limited to
65,536 bytes. It is then normalized using the ordinary-object rules:

- an effective path beginning with `/` is rejected before component
  normalization;
- one or more leading `./` components are removed;
- empty components and `.` components are removed;
- repeated `/` bytes are collapsed;
- a trailing `/` on a directory entry is ignored;
- any `..` component is rejected;
- a path that becomes empty is rejected.

Backslash has no separator semantics. `/` is the only path separator.

A symbolic-link target is preserved literally and participates in
`ObjectHash`. It may be absolute or contain `..`; those bytes describe the
link rather than an extraction destination. Materialization must nevertheless
never follow a symbolic link while resolving the destination of a later entry.

## Numeric fields and checksums

Standard ustar octal encoding is accepted for numeric fields. GNU base-256
encoding is additionally accepted for a regular-file size when the value does
not fit in the ustar octal field. A publisher uses base-256 only in that case.

Sizes must be non-negative and fit in an unsigned 64-bit integer. Offset and
padding calculations must reject integer overflow. Mode and other numeric
fields must be syntactically valid even when their values do not participate
in object identity.

Every header, including a GNU extension header, must have a valid standard tar
checksum. Readers use the unsigned-byte checksum calculation and reject a
header that requires a non-standard signed-byte interpretation.

## Metadata and normalization

For a regular file, only whether any executable bit is set participates in the
logical object:

```text
executable = mode & 0o111 != 0
```

Directory and symbolic-link modes do not participate in identity. uid, gid,
uname, gname, mtime, and all other non-structural header fields are ignored
after syntactic validation. ACLs, extended attributes, and other metadata
extensions cannot be transported by this profile.

The reader applies the storage-normalization rules specified by
[`OBJECT.md`](OBJECT.md), including canonical atime and mtime for every
materialized node. It does not preserve ignored tar metadata.

## Tree construction

The tar stream describes the children of an implicit root directory. It does
not contain a logical entry for that root.

Entries may occur in any order. Missing parent directories are synthesized.
An explicit directory entry may repeat an already explicit or synthesized
directory entry and has no additional effect. A repeated regular-file or
symbolic-link path is rejected. Any conflict between entry kinds is rejected,
including a regular file or symbolic link used as the parent of another path.

After path normalization, child entries are ordered by raw name bytes for
`ObjectHash` computation. Archive entry order does not participate in object
identity.

## Padding and archive termination

Every header and data area uses standard 512-byte tar blocks. Bytes between the
end of an entry's declared data and the next block boundary must be zero.

The archive ends with exactly two all-zero 512-byte blocks followed immediately
by the end of the decoded object payload. One zero block is insufficient.
Additional zero blocks, concatenated archives, and any trailing bytes are
rejected. An empty directory is represented by the two terminating blocks and
no logical entries.

These bytes do not participate directly in `ObjectHash`, but requiring one
termination form prevents ignored or ambiguously interpreted data from being
carried by a verified object.

## Resource limits

Parsing and hashing are streaming operations. A reader does not need to retain
regular-file contents in memory, but it must retain or externally sort enough
normalized structural information to compute a directory hash independently
of archive entry order.

A reader enforces the exact version 1 limits listed in
[`MASTER.md`](MASTER.md) for:

- total decoded size;
- individual regular-file size;
- number of entries;
- path and symbolic-link-target lengths;
- tree depth;
- in-memory structural metadata.

Limits are checked before allocation where possible and throughout decoding.
The structural-byte limit is the sum of the normalized effective path length
and, for symbolic links, literal target length over all logical entries. GNU
extension headers do not count as additional logical entries. Exceeding any
format limit rejects the object without publishing a partial result.

## Reader and materializer invariant

One parser applies this profile and produces normalized entries. Those same
entries drive both `ObjectHash` computation and safe staging-tree
materialization:

```text
decoded tar bytes
    -> validated normalized entries
        -> ObjectHash
        -> canonical staging directory
```

Hashing with one tar parser and subsequently invoking a general-purpose
`tar -xf` implementation is not conforming. Extraction operates relative to a
staging-directory descriptor, rejects traversal, and never follows a symlink
created by an earlier entry.

The staging directory is published only after the complete tar stream,
terminator, decoded size, enclosing CBOR value, and expected `ObjectHash` have
all been verified.

## Standards

- POSIX ustar archive format: POSIX.1-1988 and later tar specifications.
- GNU LongName and LongLink: GNU tar archive extensions.
- Ordinary-object identity: Bobr
  [Filesystem Object Hashing](../docs/FSOBJ_HASH.md).
