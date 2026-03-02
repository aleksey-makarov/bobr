# mbuild-fetch

`mbuild-fetch` is a builder backend for downloading URL artifacts with fixed hashes.

## Recipe Type

`type = "fetch"`

Required fields:
- `url`: source URL
- `hash`: `md5:<32-hex>` or `sha256:<64-hex>`

Optional fields:
- `unpack`: whether to extract archive content (default: `true`)
- `archive_format`: optional override for extraction format; one of `"tar-gz"`, `"tar-xz"`, `"tar-bz2"`, `"zip"`
- `artifact_kind`: defaults to `"source-tree"` when `unpack = true`, otherwise `"fetched-file"`
- `outputs`: output artifact ids (defaults to current artifact name)

## Behavior

On `build`, the builder:
1. resolves `.mbuild` layout and ensures directories exist;
2. downloads the URL with redirect limit 10;
3. verifies content hash;
4. caches the blob at `.mbuild/fetch/cache/sha256/<hex>.blob`;
5. publishes each output into:
   - `.mbuild/objects/<id>` (extracted directory when `unpack = true`, raw file otherwise),
   - `.mbuild/meta/<id>.ncl`,
   - `.mbuild/refs/<id>` symlink to `../objects/<id>`.

## Notes

- Cache is keyed by hash, so changed content under the same URL is rejected by hash mismatch.
- For `unpack = true`, format is selected in this order: explicit `archive_format`, magic bytes, URL extension.
- Supported extraction formats: `tar.gz`/`tgz`, `tar.xz`, `tar.bz2`, `zip`.
- For `zip`, extraction uses enclosed paths only and rejects unsafe paths.
- After extraction, if the output root contains exactly one top-level directory, it is normalized
  away so `.mbuild/objects/<id>` points to the actual source tree root.
