# Rootfs Builders

## Summary

`mbuild` currently implements two filesystem-related builders:

- `Tree`: realize text files and explicit empty directories as one file object or
  one installable directory object
- `Ext4Rootfs`: compose installable directory objects into one ext4 rootfs image

`Tree` is a direct authoring path:

- the builder accepts generated tree data embedded in `config.tree`
- it stages UTF-8 text files and explicit empty directories
- it publishes either one file object or one directory object, depending on the
  tree shape

`Ext4Rootfs` is a direct composition path:

- the builder reads installable directory objects from the store
- it applies install ownership rules from each input's `meta.install`
- it merges those filesystem contributions in memory
- it writes one ext4 image file directly as the realized result

There is no intermediate composed directory object published to the store.

## `Tree`

`Tree` accepts this config:

```json
{
  "tree": {
    "entries": [
      {
        "type": "dir",
        "path": "etc"
      },
      {
        "type": "file",
        "path": "etc/hostname",
        "text": "mbuild\n",
        "executable": false
      }
    ]
  },
  "install": {
    "owners": [
      { "path": "**", "uid": 0, "gid": 0 }
    ]
  }
}
```

Inputs:

- none

Current behavior:

- accepts only explicit `file` and `dir` entries
- file entries carry UTF-8 text and one `executable` flag
- parent directories for file entries are created automatically
- if the tree contains exactly one top-level file entry, the result is a file object
- otherwise the result is a directory object
- `install` is rejected for file output and required for directory output

Current limitations:

- tree entries currently support only UTF-8 text files and explicit empty directories
- symlinks, binary files, and richer file mode control are not yet supported

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
