# Rootfs Builders

## Summary

`mbuild` currently implements two filesystem-related builders:

- `Tree`: realize text files, symlinks, and explicit directories as one file object or
  one installable directory object
- `Ext4Rootfs`: compose installable directory objects into one ext4 rootfs image

`Tree` is a direct authoring path:

- the builder accepts generated tree data in `config.tree`
- generated `*-tree.ncl` modules import UTF-8 text from the adjacent `*-tree-src`
  tree instead of embedding file contents inline
- it stages UTF-8 text files, symlinks, and explicit directories
- it publishes either one file object or one directory object, depending on the
  tree shape

`Ext4Rootfs` is a direct composition path:

- the builder reads installable directory objects from the store
- it applies install rules from each input's `meta.install`
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
        "type": "symlink",
        "path": "bin",
        "target": "usr/bin"
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
    "rules": [
      {
        "path": "**",
        "attrs": {
          "uid": 0,
          "gid": 0,
          "directory_mode": 493,
          "regular_file_mode": 420,
          "executable_file_mode": 493,
          "symlink_mode": 511
        }
      }
    ]
  }
}
```

Inputs:

- none

Current behavior:

- accepts explicit `file`, `dir`, and `symlink` entries
- file entries carry UTF-8 text and one `executable` flag
- symlink entries carry one literal target string
- parent directories for file entries are created automatically
- if the tree contains exactly one top-level file entry, the result is a file object
- otherwise the result is a directory object
- `install` is rejected for file output and required for directory output
- `install.rules` uses path selectors with partial field overrides
- authoring usually starts with one broad `**` rule carrying full defaults, then
  adds narrower overrides
- when authoring `*-tree-src`, empty directories must contain `.gitkeep`; the generator ignores `.gitkeep` and still emits an empty `dir` entry
- codegen staleness checks cover tree structure, symlink targets, and executable bits;
  text file contents are read at Nickel import time

Current limitations:

- tree entries currently support only UTF-8 text files, symlinks, and explicit directories
- binary files and richer file mode control are not yet supported

## `Ext4Rootfs`

`Ext4Rootfs` accepts this config:

```json
{
  "size_mib": 256,
  "label": "rootfs"
}
```

Inputs:

- one or more named installable directory inputs
- contribution order follows lexical input name order

Current behavior:

- requires every input to resolve to a directory object
- requires every input to carry valid `meta.install.rules`
- scans files, directories, and symlinks from all inputs
- resolves install attributes per path using full coverage and field-wise last-match-wins semantics
- merges all filesystem contributions with strict conflict checking
- writes one ext4 image file directly from the merged filesystem state

Conflict policy:

- matching `directory/directory` overlap is allowed only when final `mode`, `uid`, and `gid` are identical
- any other duplicate path is rejected

Install policy:

- every path contributed by an input must match at least one `install.rules` rule
- the last matching rule for each individual field sets the final resolved value
- final required fields must be fully resolved for the installed entry kind:
  - directories: `uid`, `gid`, `directory_mode`
  - regular files: `uid`, `gid`, plus `regular_file_mode` or
    `executable_file_mode` depending on the payload executable bit
  - symlinks: `uid`, `gid`, `symlink_mode`
- missing resolved fields are rejected as a composition error
- the builder does not trust source tree mode bits for final install mode
- for regular files, the payload executable bit selects between
  `regular_file_mode` and `executable_file_mode`
- all mode fields are full final unix modes and may include special bits such as
  `setuid`, `setgid`, and `sticky`

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
