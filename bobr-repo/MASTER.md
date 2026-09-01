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

- identifies the repository and its wire-format version;
- provides a monotonically increasing publication revision;
- names the HTTPS base URL of immutable repository data;
- describes the configured cyclic slots without fixing their number;
- identifies the active slot and the generation of every slot;
- authenticates one mapping index and one content list for every slot by their
  SHA-256 digests;
- carries temporary retired metadata roots needed for safe garbage collection;
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
repository_id
revision
data_base_url
active_slot
slots = [slot, ...]
retention = [retired root, ...]
```

Unknown fields are rejected in repository format version 1. An incompatible
payload change requires another `repository_format` value and a corresponding
schema.

### Repository format

`repository_format` describes the remote repository wire format, not the Bobr
implementation version. Its value is exactly `1` for this document.

### Repository identity

`repository_id` is a random 16-byte identifier generated once when the
repository is initialized. It is never reused for another logical repository.

A trusted client configuration binds the master URL and pinned public keys to
an expected repository identity. A valid signature from a key used for another
repository must not silently change that identity.

### Revision

`revision` is an unsigned 64-bit publication counter. The first published
master uses revision 1. A publisher increments it by exactly one for every
successful replacement of `/master`.

A client persists the greatest accepted revision for each repository identity:

- a smaller revision is rejected as a rollback;
- the same revision with different master bytes is rejected as an inconsistent
  publication;
- a larger valid revision is accepted, even if the client did not observe all
  intermediate revisions.

Revision tracking cannot prove that an origin is serving the newest existing
master to a new client. Version 1 does not define expiration metadata or a
trusted-time-based freeze-attack policy.

### Data base URL

`data_base_url` is the root for immutable data. Repository keys are relative to
it:

```text
m/<lowercase MappingIndexHash hex>
l/<lowercase ContentListHash hex>
o/<lowercase ObjectHash hex>
f/<lowercase FsFileHash hex>
```

Digests are represented as raw 32-byte strings inside CBOR and as lowercase
hexadecimal strings only in URLs.

### Slots

`slots` is a non-empty array. Its length is the configured slot count; the
protocol does not prescribe a particular count. Array order defines cyclic
rotation order.

Every slot contains:

- `id`: an unsigned 32-bit identifier, unique within the array;
- `generation`: an unsigned 64-bit counter;
- `mappings`: SHA-256 of the immutable mapping index for this slot;
- `content`: SHA-256 of the immutable content list for this slot.

`active_slot` must equal the `id` of exactly one slot in the array. All other
slots are sealed. An ordinary client uses the union of all slots and does not
otherwise need to distinguish active from sealed slots.

A slot generation is incremented when that slot is cleared and rebuilt during
rotation. Incremental additions to an already-active slot update its mapping
index, content list, and master revision without changing the generation.

Changing the number or order of slots is an explicit repository
reconfiguration. It is not an incidental option of an ordinary publication.
Removing a slot can make its exclusively referenced content eligible for
garbage collection and therefore observes the same retention rules as a normal
rotation.

An initialized repository uses the canonical empty mapping index and content
list for slots that have not yet been populated. Consequently every slot always
has both metadata digests; optional or partially initialized descriptors are
not needed.

### Retention roots

`retention` is an array of metadata roots whose removal is delayed by the
garbage collection grace period. Every entry contains:

- `mappings`: the SHA-256 digest of a superseded mapping index;
- `content`: the SHA-256 digest of the matching superseded content list;
- `retain_until`: an absolute Unix timestamp in seconds.

When a publication removes a mapping-index/content-list pair from the current
slot descriptors, the publisher adds that pair to `retention`. Until
`retain_until`, garbage collection treats the old mapping index, old content
list, and every payload named by that content list as live. This permits a
client that already fetched an older master to finish its operation.

Retention roots are part of the signed master payload rather than separate
objects. The master therefore contains all durable state needed to recover safe
publisher and garbage-collection operation after loss of local publisher
state. Ordinary readers parse the entries but do not fetch retained indexes or
lists for normal lookup.

The array is sorted lexicographically by the raw `mappings` digest and then by
the raw `content` digest. A pair occurs at most once; when the same pair would
be retained more than once, the publisher keeps the greatest `retain_until`.
Expired entries are removed only by publishing a new master that no longer
contains them.

An empty array means that no superseded roots are currently retained. Its size
is proportional to the number of publications within one grace period, not to
the number of payload objects. Implementations impose a master size limit and
must fail publication rather than silently discard unexpired roots.

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
8. Check the configured repository identity and the stored revision.
9. Fetch missing current mapping indexes and content lists by their digests.
10. Verify every fetched immutable metadata file before using it. A publisher
    or GC additionally fetches retained indexes and lists when it needs to
    compute the complete live set.

No mapping from an unverified master may be used, even when the eventual
content would be checked by `ObjectHash`.

## Publication procedure

For one publication the sole publisher:

1. Reads and verifies the current master.
2. Computes the new slot metadata and the updated array of retention roots.
3. Uploads missing immutable content under `o/` and `f/`.
4. Uploads the new immutable mapping index and content list under `m/` and
   `l/`.
5. Constructs the next payload with revision `previous + 1`.
6. Encodes the payload and COSE object deterministically and signs it with the
   configured Ed25519 key.
7. Replaces `/master` last using the object store's atomic single-object
   replacement operation.

If the publisher fails before the last step, it may leave unreachable immutable
objects but cannot expose a partially committed repository state. If it fails
after the replacement, the signed master contains both current and retained
roots required to resume operation.

## Trust boundaries

The signature authenticates the master and, transitively, the digests of its
immutable metadata. It does not make the HTTPS origin trusted for correctness:
the origin may still withhold data or serve an older master.

Mapping indexes from the repository may be used as trusted `BuildKey` and
`ReuseKey` answers only when client configuration grants that capability to the
repository. Content is independently verified by its logical `ObjectHash` or
`FsFileHash` after decoding, regardless of mapping trust.

Private signing keys and S3 credentials are not repository objects. They remain
outside the public bucket and outside `/master`.

## Standards

- CBOR and deterministic encoding: RFC 8949.
- CDDL: RFC 8610.
- COSE structures and processing: RFC 9052.
- COSE algorithms, including EdDSA: RFC 9053.
- Ed25519: RFC 8032.
