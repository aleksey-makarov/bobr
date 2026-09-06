# Remote repository filesystem file

Status: format design draft.

This document specifies the proposed repository format version 1 wire format
of immutable filesystem files stored under `f/<FsFileHash>`. The normative CDDL
schema is in [`fs-file.cddl`](fs-file.cddl).

## Identity

The key has this form:

```text
f/<lowercase FsFileHash hex>
```

An fs-file is always one regular file. Its identity includes its complete
logical ownership and mode as well as its size and byte content. It is distinct
from the [`ObjectHash`](OBJECT.md) domain used for ordinary objects.

Repository format version 1 computes `FsFileHash` as:

```text
sha256(
    b"bobr:fs-file:v1\0"
    || uid:u32be
    || gid:u32be
    || mode:u32be
    || size:u64be
    || sha256(file_bytes)
)
```

Here `mode` is in `0..=0o7777`, `size` is the decoded file size, and
`file_bytes` is the decoded payload. All mode bits in that range participate in
identity; they are not reduced to an executable flag. The CBOR envelope and
compression do not participate in `FsFileHash`.

An fs-file key is write-once. A publisher that finds an existing
`f/<FsFileHash>` reuses it and must not replace it with another equivalent
encoding or compression choice. This gives the immutable URL stable response
bytes and cache validators.

## HTTP representation

An fs-file response uses:

```text
Content-Type: application/vnd.bobr.repository-fs-file+cbor
Cache-Control: public, max-age=31536000, immutable
Content-Encoding: absent
```

The repository may use a longer freshness lifetime. A client must still
validate the complete decoded identity before admitting the file to its
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
reader can parse the small metadata map and byte-string length, then stream
exactly that many bytes through the selected decoder without holding the file
in memory. A compliant reader must not require one allocation proportional to
the payload size.

The envelope contains neither an embedded format version nor an embedded
`FsFileHash`. The signed master selects `repository_format`, and the requested
URL supplies the expected identity. Any incompatible change to this format or
its CDDL schema requires a new repository format version.

## Metadata

Metadata has this logical form:

```text
{
    "uid": uint32,
    "gid": uint32,
    "mode": 0..0o7777,
    "compression": "identity" | "zstd",
    "decoded_size": uint64,
}
```

`uid`, `gid`, `mode`, and `decoded_size` all participate directly in the
`FsFileHash` computation. Producing fewer or more decoded bytes than
`decoded_size` declares is an error.

`decoded_size` also supports progress reporting and resource limits. A reader
must not use an untrusted value to allocate that amount of memory. The encoded
payload length is already carried by the definite-length CBOR byte string and
is not repeated in metadata.

## Payload and compression

After decompression, the payload is exactly the regular file's byte content.
Repository format version 1 defines two compression values:

- `identity`: the encoded payload is already the file bytes;
- `zstd`: the encoded payload is exactly one standard Zstandard frame whose
  decoded bytes are the file bytes.

A zstd payload must not require an external dictionary. The decoder must
consume the complete encoded payload, finish exactly at its end, and produce
exactly `decoded_size` bytes. A trailing frame or trailing non-frame data is an
error.

The publisher chooses compression independently for each fs-file. Small or
already-compressed files can remain `identity`; larger compressible files can
use zstd. The choice does not affect `FsFileHash`.

A deterministic definite-length byte string requires the publisher to know the
encoded payload size before writing the CBOR prefix. For zstd, a publisher may
first write the compressed representation to a temporary file and then stream
that file into the CBOR object or multipart upload. This requires temporary
disk space but no allocation proportional to the file size.

## Storage representation

Unlike an ordinary object, an fs-file preserves the exact identity metadata
declared by the envelope:

- uid is exactly `metadata.uid`;
- gid is exactly `metadata.gid`;
- mode is exactly `metadata.mode`.

This includes all permission, executable, set-user-ID, set-group-ID, and sticky
bits representable by `0o7777`. They must not be normalized to ordinary-object
modes.

Metadata not represented by `FsFileHash` is normalized. Bobr stamps both atime
and mtime to `315532800.000000000`, which is 1980-01-01 00:00:00 UTC and the
current `CANONICAL_TIMESTAMP`. Other timestamps, ACLs, extended attributes,
file capabilities, and inode topology are not part of fs-file identity.

The normalization order matters when importing ownership that differs from the
host user:

1. Write the decoded bytes to a private staging file.
2. Set canonical atime and mtime while the staging file is still owned by the
   importing process.
3. Change ownership to the declared uid and gid.
4. Set the declared mode.
5. Verify the resulting file metadata and identity.
6. Atomically publish it in the working store.

Applying arbitrary logical ownership normally requires Bobr's namespace
runtime. Network transfer and content hashing may run asynchronously without a
namespace, but final normalization and publication must use a
namespace-capable local operation when the declared owner differs from the
host user.

## Reader procedure

A reader importing `f/<FsFileHash>`:

1. Applies the repository format encoded-content limit before parsing.
2. Parses and validates the deterministic two-element CBOR envelope and its
   metadata without allocating the payload size.
3. Reads exactly the definite byte-string length through the selected
   decompressor while enforcing resource and decoder-window limits.
4. Writes decoded bytes to a private staging file while computing their
   SHA-256 digest and length.
5. Requires the decoded length to equal `decoded_size`.
6. Computes `FsFileHash` from the declared uid, gid, mode, decoded size, and
   content digest.
7. Requires the result to equal the expected `FsFileHash`.
8. Requires the payload and enclosing CBOR value to end exactly where declared.
9. Applies canonical timestamps and the declared ownership and mode through the
   appropriate local runtime.
10. Verifies the staged regular file again and atomically publishes it in the
    working store.

Any malformed CBOR, unknown metadata, out-of-range mode, unsupported
compression, size mismatch, decompression failure, trailing data, metadata
application failure, or fs-file-hash mismatch rejects the complete file.

## Manifest integration

An fs-tree manifest regular-file entry contains only its path and
`FsFileHash`:

```json
{"p":"usr/bin/tool","t":"f","h":"<FsFileHash>"}
```

The hash binds the file bytes and the uid, gid, and mode recovered from this
envelope. A client first confirms that the hash appears in a current
filesystem-file list under `lf/<FsFileListHash>`, imports and verifies the
corresponding `f/<FsFileHash>`, and then materializes the fs-tree by hardlinking
the canonical local fs-file into place.

## Standards

- CBOR and deterministic encoding: RFC 8949.
- CDDL: RFC 8610.
- Zstandard: RFC 8878.
- Fs-tree integration: Bobr
  [Filesystem trees](../docs/FS_TREE.md) and
  [fs-tree Manifest](../docs/FS_TREE_MANIFEST.md).
