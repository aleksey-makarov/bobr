# Rootfs Builders

## Summary

`mbuild` currently implements one filesystem composition builder:

- `Ext4Rootfs`: compose installable directory objects into one ext4 rootfs image

This is a direct composition path:

- the builder reads installable directory objects from the store
- it applies install ownership rules from each input's `meta.install`
- it merges those filesystem contributions in memory
- it writes one ext4 image file directly as the realized result

There is no intermediate composed directory object published to the store.

## `Ext4Rootfs`

`Ext4Rootfs` accepts this config:

```json
{
  "size_mib": 256,
  "label": "rootfs"
}
```

Inputs:

- repeated `inputs`: one or more installable directory objects

Current behavior:

- requires every input to resolve to a directory object
- requires every input to carry valid `meta.install.owners`
- scans files, directories, and symlinks from all inputs
- applies ownership rules per path using full coverage and last-match-wins semantics
- merges all filesystem contributions with strict conflict checking
- writes one ext4 image file directly from the merged filesystem state

Conflict policy:

- matching `directory/directory` overlap is allowed only when final `mode`, `uid`, and `gid` are identical
- any other duplicate path is rejected

Ownership policy:

- every path contributed by an input must match at least one `install.owners` rule
- the last matching rule sets `uid` and `gid`
- missing coverage is rejected as a composition error

The realized result payload is one regular file containing an ext4 filesystem image.

The current realized result metadata is empty:

- `{}`

## Current Limitations

`Ext4Rootfs` currently supports only filesystem content already supported by the object store:

- regular files
- directories
- symlinks

Current limitations:

- special files such as block devices, character devices, FIFOs, and sockets are not supported
- the ext4 image size must be provided explicitly through `size_mib`
- the builder does not yet serve as the backend for `Image`; OCI-based composition remains a separate path
