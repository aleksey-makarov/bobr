# Remote repository object

Status: format design draft.

This document specifies the proposed repository format version 1 wire format
of immutable objects stored under `o/<ObjectHash>`. The normative CDDL schema
is in [`object.cddl`](object.cddl).

## Identity

The key has this form:

```text
o/<lowercase ObjectHash hex>
```

`ObjectHash` identifies the decoded logical filesystem object. It is not the
SHA-256 digest of the CBOR representation, the encoded payload, or the decoded
tar bytes. The envelope and compression do not participate in object identity.

The decoded object is one of the ordinary object kinds accepted by Bobr's
filesystem object hashing rules:

- a regular file, whose identity includes its bytes and executable state;
- a directory containing regular files, directories, and symbolic links.

Root symbolic links and other filesystem kinds are not supported. The
normative filesystem identity and tar normalization rules are specified in
[Filesystem Object Hashing](../docs/FSOBJ_HASH.md).

An object key is write-once. A publisher that finds an existing
`o/<ObjectHash>` reuses it and must not replace it with another equivalent
encoding or compression choice. This gives the immutable URL stable response
bytes and cache validators even though more than one wire representation could
decode to the same logical object.

## HTTP representation

An object response uses:

```text
Content-Type: application/vnd.bobr.repository-object+cbor
Cache-Control: public, max-age=31536000, immutable
Content-Encoding: absent
```

The repository may use a longer freshness lifetime. A client must still
validate the decoded logical identity before admitting the object to its
working store.

## CBOR envelope

The response body is one untagged CBOR array with exactly two elements:

```text
[
    metadata,
    payload: bstr,
]
```

It uses the core deterministic encoding requirements of RFC 8949 section
4.2.1. Indefinite-length arrays, maps, text strings, and byte strings are
forbidden. Integers and lengths use their shortest encoding, map keys are in
deterministic order, unknown fields are rejected, and no bytes may follow the
array.

The payload is a definite-length CBOR byte string and is the final value in the
array. Its length is the encoded payload length, before any decompression. A
reader can parse the small metadata map and the byte-string length, then stream
exactly that many bytes through the selected decoder without holding the
payload in memory. A compliant reader must support this streaming path and must
not require one allocation proportional to the payload size.

The envelope contains neither an embedded format version nor an embedded
`ObjectHash`. The signed master selects `repository_format`, and the requested
URL supplies the expected identity. Any incompatible change to this format or
its CDDL schema requires a new repository format version.

## Common metadata

Both object kinds contain:

- `kind`: the logical root kind;
- `compression`: the representation of the encoded payload;
- `decoded_size`: the exact number of payload bytes after decompression.

`decoded_size` is an unsigned 64-bit integer. It supports progress reporting,
resource limits, and exact decoder validation. A reader must not use an
untrusted value to allocate that amount of memory. Producing fewer or more
decoded bytes than declared is an error.

The encoded payload length is already carried by the definite-length CBOR byte
string and is not repeated in metadata.

## File object

File metadata has this logical form:

```text
{
    "kind": "file",
    "executable": true | false,
    "compression": "identity" | "zstd",
    "decoded_size": uint64,
}
```

After decompression, the payload is exactly the regular file's byte content.
The reader computes the file object hash from those bytes, their length, and
the declared executable state, and requires it to equal the `ObjectHash` in
the requested key.

For a non-executable file this is exactly the SHA-256 digest of the decoded
bytes. An executable file uses Bobr's tagged file hash. The `executable` field
is required for a file and is forbidden for a directory.

## Directory object

Directory metadata has this logical form:

```text
{
    "kind": "directory",
    "archive": "tar",
    "compression": "identity" | "zstd",
    "decoded_size": uint64,
}
```

After decompression, the payload is one tar stream describing the directory
contents. The stream must satisfy the repository
[directory tar profile](TAR.md). The reader applies that profile to obtain the
normalized tree defined by
[Filesystem Object Hashing](../docs/FSOBJ_HASH.md), computes its hash, and
requires it to equal the `ObjectHash` in the requested key.

The object identity is independent of tar header layout, entry order, implicit
parent directories, ownership, timestamps, and ignored mode bits. A publisher
is not required to produce one canonical sequence of tar bytes. The immutable
write-once object key selects the first valid wire representation published for
that logical object.

Repository format version 1 deliberately accepts only POSIX ustar logical
entries and the GNU LongName and LongLink extensions needed for long raw byte
strings. PAX, sparse-file formats, and other extensions are rejected. The full
entry, path, checksum, padding, termination, and safe-materialization rules are
specified in [`TAR.md`](TAR.md). A repository implementation must use the same
accepted normalized model for hashing and materialization; invoking a
general-purpose system `tar -xf` is not sufficient.

## Compression

Repository format version 1 defines two compression values:

- `identity`: the encoded payload is already the file bytes or tar stream;
- `zstd`: the encoded payload is exactly one standard Zstandard frame whose
  decoded bytes are the file bytes or tar stream.

A zstd payload must not require an external dictionary. The decoder must
consume the complete encoded payload, finish exactly at its end, and produce
exactly `decoded_size` bytes. A trailing frame or trailing non-frame data is an
error.

The publisher chooses compression independently for each object. It can leave
already-compressed inputs as `identity` and use zstd where it provides a useful
reduction. The choice does not affect `ObjectHash`.

A deterministic definite-length byte string requires the publisher to know the
encoded payload size before writing the CBOR prefix. For zstd, a publisher may
first write the compressed representation to a temporary file and then stream
that file into the CBOR object or multipart upload. This requirement consumes
temporary disk space but does not require holding the object in memory.

## Storage normalization

Successful decoding reconstructs the canonical ordinary-object storage
representation. Only metadata represented by `ObjectHash` is retained from
the transported object; all other filesystem metadata is normalized.

For a root file and every regular file in a directory:

- executable state is false or true according to the file metadata or tar mode
  expression `mode & 0o111 != 0`;
- the normalized mode is `0644` when non-executable and `0755` when
  executable.

For every directory, the normalized mode is `0755`. All entries receive the
uid and gid of the user that owns the working store. Symbolic-link targets are
preserved literally because they participate in object identity; symbolic-link
ownership is normalized to the same user and group. ACLs, extended attributes,
set-id bits, and all other transported metadata are discarded.

The canonical timestamp for ordinary objects is:

```text
CANONICAL_TIMESTAMP = 315532800.000000000
                      # 1980-01-01 00:00:00 UTC
```

Both atime and mtime are set to this value for:

- a root file object;
- every regular file in a directory object;
- every directory, including the root directory object;
- every symbolic link, without following the link.

Ownership and modes are normalized before timestamps. Directory timestamps are
applied after all descendants have been created, in post-order, so subsequent
materialization does not change them. Symbolic-link timestamps are applied with
no-follow semantics. Failure to apply the canonical timestamps rejects the
import.

ctime, birth time, and any other filesystem-maintained timestamps are not
controllable portable object metadata and are outside the canonical storage
representation. A later access may also update atime according to the working
store filesystem's mount policy; neither such change participates in
`ObjectHash` or invalidates the object.

Normalization is part of ordinary-object storage semantics, not an optional
security transformation. Two accepted representations with the same
`ObjectHash` materialize as the same canonical local object.

The same contract applies when an ordinary object reaches the working store
through a local content source rather than this remote representation. A copy
transport normalizes its staging object before publication. A transport that
reuses the source representation directly may do so only when that
representation already satisfies this contract; otherwise the transfer is
rejected. Content-source choice must not determine the resulting stored modes,
ownership, or initial timestamps.

## Reader procedure

A reader importing `o/<ObjectHash>`:

1. Applies the repository format encoded-content limit before parsing.
2. Parses and validates the deterministic two-element CBOR envelope and its
   metadata without allocating the payload size.
3. Reads exactly the definite byte-string length through the selected
   decompressor while enforcing resource and decoder-window limits.
4. Counts decoded bytes and requires the count to equal `decoded_size`.
5. For a file, hashes the decoded bytes with the declared executable state and
   writes a normalized staging file.
6. For a directory, validates the complete repository tar profile, hashes the
   normalized tree, and safely materializes a normalized staging directory
   from the same parsed entries.
7. Requires the resulting hash to equal the expected `ObjectHash`.
8. Requires the payload and enclosing CBOR value to end exactly where declared.
9. Atomically publishes the verified staging object into the working store.

Any malformed CBOR, unknown metadata, unsupported object kind, unsupported
compression, size mismatch, decompression failure, invalid tar entry, unsafe
path, trailing data, or object-hash mismatch rejects the complete object.

## Fs-tree manifests

An fs-tree manifest is an ordinary non-executable file object in this format.
It uses `kind = "file"`, `executable = false`, and its canonical JSONL manifest
bytes as the decoded payload. Once the manifest is verified, its referenced
filesystem files are resolved independently through `f/<FsFileHash>`.

The `f/` wire format is outside the scope of this document. Unlike an ordinary
object, an fs-file has a separate identity that includes uid, gid, and its full
mode. Its wire representation is specified in
[`FS_FILE.md`](FS_FILE.md) and [`fs-file.cddl`](fs-file.cddl).

## Standards

- CBOR and deterministic encoding: RFC 8949.
- CDDL: RFC 8610.
- Zstandard: RFC 8878.
- Directory transport tar profile: [`TAR.md`](TAR.md).
- Tar identity and normalization: Bobr
  [Filesystem Object Hashing](../docs/FSOBJ_HASH.md).
