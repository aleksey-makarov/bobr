# mbuild-fetch

`mbuild-fetch` is a builder backend for downloading URL artifacts with fixed hashes.

## Recipe Type

`type = "fetch"`

Required fields:
- `url`: source URL
- `hash`: `md5:<32-hex>` or `sha256:<64-hex>`
- `layout`: `"file"` or `"archive-tree"`

Optional fields:
- `archive_format`: required when `layout = "archive-tree"`; one of `"tar-gz"`, `"tar-xz"`, `"zip"`
- `artifact_kind`: defaults to `"fetched-file"` for `file`, `"source-tree"` for `archive-tree`
- `outputs`: output artifact ids (defaults to current artifact name)

## Behavior

On `build`, the builder:
1. resolves `.mbuild` layout and ensures directories exist;
2. downloads the URL with redirect limit 10;
3. verifies content hash;
4. caches the blob at `.mbuild/fetch/cache/sha256/<hex>.blob`;
5. publishes each output into:
   - `.mbuild/objects/<id>` (file or extracted directory),
   - `.mbuild/meta/<id>.ncl`,
   - `.mbuild/refs/<id>` symlink to `../objects/<id>`.

## Notes

- Cache is keyed by hash, so changed content under the same URL is rejected by hash mismatch.
- For `zip`, extraction uses enclosed paths only and rejects unsafe paths.
