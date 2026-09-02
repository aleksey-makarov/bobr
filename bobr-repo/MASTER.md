# Remote repository master

Status: format design draft.

This document specifies the purpose and proposed version 1 wire format of the
mutable `/master` object in a Bobr remote repository. The normative CDDL schema
of its payload is in [`master-payload.cddl`](master-payload.cddl).

## Purpose

`/master` is the small, mutable, authenticated root of a remote repository. It
does not contain build results or large indexes. It identifies the repository
state that a reader must use and authenticates the immutable metadata reachable
from that state.

In particular, `/master`:

- declares its wire-format version;
- names the HTTPS base URL of immutable repository data;
- describes the current and temporarily retained slot states without fixing
  the number of current slots;
- authenticates one build index, one reuse index, one object list, and one
  filesystem-file list for every slot state by their SHA-256 digests;
- carries the retention deadline for each retired slot state;
- identifies which already-pinned signing key produced the signature.

The master is the publication commit point. A publisher uploads all new
immutable content and metadata first and replaces `/master` only after every
referenced object is available. A reader therefore observes either the old
complete repository state or the new complete repository state.

## Object location and HTTP representation

The configured master URL is stable and need not share an origin with the
immutable repository data. Its conventional path is `/master`.

The HTTP response uses:

```text
Content-Type: application/cose; cose-type="cose-sign1"
Cache-Control: no-cache
```

`no-cache` permits local storage but requires revalidation before reuse. An
origin should provide an `ETag` or `Last-Modified` validator so an unchanged
master normally produces a small `304 Not Modified` response.

The signed payload contains `data_base_url`. Immutable object keys are resolved
relative to that URL, so moving data does not require changing the stable
master URL or the client's trust configuration. `data_base_url` must be an
absolute HTTPS URL, must end in `/`, and must not contain user information, a
query, or a fragment.

## COSE profile

`/master` is a tagged `COSE_Sign1` object as defined by RFC 9052. The CBOR tag
18 is required. Detached payloads are not permitted.

The protected header map contains exactly these parameters:

```text
alg:          EdDSA (-8)
content type: application/vnd.bobr.repository-master+cbor
kid:          opaque non-empty byte string
```

The signing key is Ed25519. The `kid` value only selects one of the public keys
already pinned in the client configuration. It is not a certificate, does not
carry trust by itself, and must never cause a client to download and trust an
unconfigured key.

The unprotected header map is empty. Version 1 puts no security-relevant or
semantic information in unsigned COSE headers. External additional
authenticated data is the empty byte string.

The payload is embedded as a byte string and contains one deterministic-CBOR
encoding of `bobr-repository-master-payload`. The signature is the 64-byte
Ed25519 signature produced according to the standard COSE `Signature1`
construction. Bobr does not define a separate signature field or its own byte
concatenation rules.

The complete COSE object, its protected header map, and its payload must satisfy
the core deterministic encoding requirements of RFC 8949 section 4.2.1. A
version 1 implementation rejects indefinite-length items, non-shortest integer
or length encodings, duplicate map keys, incorrectly ordered map keys, and
other non-deterministic encodings.

## Payload

The payload has the following logical structure:

```text
repository_format = 1
data_base_url
slots = [slot, ...]
```

Unknown fields are rejected in repository format version 1. An incompatible
payload change requires another `repository_format` value and a corresponding
schema.

### Repository format

`repository_format` describes the remote repository wire format, not the Bobr
implementation version. Its value is exactly `1` for this document.

### Data base URL

`data_base_url` is the root for immutable data. Repository keys are relative to
it:

```text
b/<lowercase BuildIndexHash hex>
r/<lowercase ReuseIndexHash hex>
lo/<lowercase ObjectListHash hex>
lf/<lowercase FsFileListHash hex>
o/<lowercase ObjectHash hex>
f/<lowercase FsFileHash hex>
```

Digests are represented as raw 32-byte strings inside CBOR and as lowercase
hexadecimal strings only in URLs.

### Immutable metadata formats

The four immutable metadata formats have no header, magic number, embedded
version, record count, padding, or delimiter. Their interpretation follows
from the field that references them in a verified master and from
`repository_format`. Any incompatible change to one of these formats requires
a new repository format version.

All hashes and keys in these files use their raw 32-byte representation. Hex
encoding is used only for repository object names. Ordering is unsigned
lexicographic ordering of those 32 bytes.

#### Build index

A build index under `b/<BuildIndexHash>` is a sequence of fixed-size 64-byte
records:

```text
BuildKey[32] ObjectHash[32]
BuildKey[32] ObjectHash[32]
...
```

Records are strictly ordered by `BuildKey`; duplicate keys are forbidden. The
file size must be divisible by 64. A zero-length file is the canonical empty
build index. `BuildIndexHash` is the SHA-256 digest of the exact file bytes.

#### Reuse index

A reuse index under `r/<ReuseIndexHash>` has the same record representation:

```text
ReuseKey[32] ObjectHash[32]
ReuseKey[32] ObjectHash[32]
...
```

Records are strictly ordered by `ReuseKey`; duplicate keys are forbidden. The
file size must be divisible by 64. A zero-length file is the canonical empty
reuse index. `ReuseIndexHash` is the SHA-256 digest of the exact file bytes.

#### Object list

An object list under `lo/<ObjectListHash>` is a sequence of raw object hashes:

```text
ObjectHash[32]
ObjectHash[32]
...
```

Hashes are strictly ordered and therefore unique. The file size must be
divisible by 32. A zero-length file is the canonical empty object list.
`ObjectListHash` is the SHA-256 digest of the exact file bytes.

#### Filesystem-file list

A filesystem-file list under `lf/<FsFileListHash>` is a sequence of raw
filesystem-file hashes:

```text
FsFileHash[32]
FsFileHash[32]
...
```

Hashes are strictly ordered and therefore unique. The file size must be
divisible by 32. A zero-length file is the canonical empty filesystem-file
list. `FsFileListHash` is the SHA-256 digest of the exact file bytes.

For normal content lookup, a client uses the union of object lists and the
union of filesystem-file lists referenced by current slots. If an
`ObjectHash` is absent from the former, or an `FsFileHash` is absent from the
latter, the repository does not advertise that content and the client need not
issue a request under `o/` or `f/`.

### Slots

`slots` is a non-empty array of current and temporarily retained slot states.
The protocol does not prescribe how many current slots a repository has. Every
entry contains:

- `serial`: an unsigned 64-bit sequence number, unique within the array;
- `build`: SHA-256 of the immutable build index for this slot;
- `reuse`: SHA-256 of the immutable reuse index for this slot;
- `object_list`: SHA-256 of the immutable object list for this slot;
- `file_list`: SHA-256 of the immutable filesystem-file list for this slot;
- `retain_until`: `null` for a current slot, or an absolute Unix timestamp in
  seconds for a retired slot.

The array is sorted by strictly increasing `serial`. At least one entry must be
current. The active slot is the current entry with the greatest `serial`; the
other current entries are sealed. An ordinary client uses build and reuse
indexes from all current entries and ignores retired entries for lookup.

`serial` identifies one immutable slot state, not a reusable physical slot.
Whenever publication changes the active slot's indexes or lists, the publisher
marks the previous active entry as retired and appends its replacement with
`serial = max(slots.serial) + 1` and `retain_until = null`. Thus changing a slot
never changes the meaning of an existing serial.

At rotation, the publisher retires the current entry with the smallest serial
and appends a fresh active entry with the next serial. The formerly active
entry remains current and thereby becomes sealed. The number of current entries
therefore remains unchanged. Repository initialization chooses that number;
later publisher runs can recover it by counting entries whose `retain_until` is
`null`.

An initialized repository uses canonical empty build and reuse indexes,
canonical empty object lists, and canonical empty filesystem-file lists for
slots that have not yet been populated. Consequently every current entry
always has all four metadata digests; optional or partially initialized
descriptors are not needed.

### Retired slot states and retention

Retirement changes a slot entry's `retain_until` from `null` to the end of the
garbage-collection grace period. The timestamp is the earliest time at which a
publisher may omit that entry from a newly published master. Reaching the
timestamp does not itself make the entry or its content dead: while the entry
is still present in the authoritative master, it remains a live root.

Garbage collection treats the build index, reuse index, both lists, every
object named by the object list, and every filesystem file named by the
filesystem-file list of every entry in `slots` as live, whether the entry is
current or retired. After a retired entry's deadline, the sole publisher may
publish a new master without that entry. Only after that publication may
garbage collection remove immutable metadata and content no longer reachable
from any remaining entry.

This grace period permits a client that already fetched an older master to
finish its operation. Ordinary readers parse retired entries but do not fetch
their indexes or lists for normal lookup.

Retention state is part of the signed master rather than separate mutable
publisher state. The bucket therefore contains all durable information needed
to resume publication and safe garbage collection after loss of local state.

The number of retired entries is proportional to the number of publications
within one grace period, not to the number of payload objects. Implementations
impose a master size limit and must fail publication rather than silently
discard unexpired entries.

## Verification procedure

A client processing `/master` performs these operations in order:

1. Apply an implementation-defined encoded-size limit before parsing.
2. Require CBOR tag 18 and the `COSE_Sign1` structure used by this profile.
3. Validate deterministic CBOR encoding and reject duplicate or unknown
   headers.
4. Require the protected algorithm, content type, and `kid`; require an empty
   unprotected map and an embedded payload.
5. Select an already-pinned Ed25519 public key by exact `kid` match.
6. Verify the COSE signature over the original embedded payload bytes. Do not
   decode and re-encode the payload for signature verification.
7. Decode the payload and validate it against `master-payload.cddl` and the
   semantic constraints in this document.
8. Treat the verified response from the configured master URL as the
   authoritative repository state. A byte-identical cached response may be
   reused after successful HTTP revalidation.
9. Fetch missing current build indexes, object lists, and filesystem-file lists
   by their digests. Fetch reuse indexes lazily when exact lookup misses and
   reuse lookup becomes possible.
10. Verify every fetched immutable metadata file before using it. A publisher
    or GC additionally fetches retired indexes and lists when it needs to
    compute the complete live set.

No mapping from an unverified master may be used, even when the eventual
content would be checked by `ObjectHash`.

## Publication procedure

For one publication the sole publisher:

1. Reads and verifies the current master.
2. Computes the replacement active-slot state, any rotation, and the updated
   retention deadlines.
3. Uploads missing immutable content under `o/` and `f/`.
4. Uploads the new immutable build index, reuse index, object list, and
   filesystem-file list under `b/`, `r/`, `lo/`, and `lf/`.
5. Constructs the next payload, using the next serial for every newly created
   slot state and retaining all unexpired retired states.
6. Encodes the payload and COSE object deterministically and signs it with the
   configured Ed25519 key.
7. Replaces `/master` last using the object store's atomic single-object
   replacement operation.

If the publisher fails before the last step, it may leave unreachable immutable
objects but cannot expose a partially committed repository state. If it fails
after the replacement, the signed master contains all current and retired roots
required to resume operation.

## Trust boundaries

The signature authenticates the master and, transitively, the digests of its
immutable metadata. It does not make the HTTPS origin trusted for correctness:
the origin may still withhold data or serve an older master.

Repository format version 1 deliberately has no monotonic revision or trusted
time mechanism. The response obtained from the configured master URL is
authoritative. Comparing its bytes or digest with a cached response detects a
change but does not distinguish a legitimate older state from a rollback. The
signature alone therefore does not prevent replay or freeze attacks by the
master origin.

Build and reuse indexes from the repository may be used as trusted `BuildKey`
and `ReuseKey` answers only when client configuration grants that capability to
the repository. Content is independently verified by its logical `ObjectHash`
or `FsFileHash` after decoding, regardless of mapping trust.

Private signing keys and S3 credentials are not repository objects. They remain
outside the public bucket and outside `/master`.

## Standards

- CBOR and deterministic encoding: RFC 8949.
- CDDL: RFC 8610.
- COSE structures and processing: RFC 9052.
- COSE algorithms, including EdDSA: RFC 9053.
- Ed25519: RFC 8032.
