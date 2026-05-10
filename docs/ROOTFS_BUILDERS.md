# Rootfs Builders

## Summary

`mbuild` currently implements four filesystem-related builders:

- `Tree`: realize text files, symlinks, and explicit directories as one file object or
  one fs-tree directory object
- `TreeMerge`: compose fs-tree directory objects into one fs-tree directory object
- `Rootfs`: compose installable legacy directory objects into one rootfs directory object
- `Ext4Rootfs`: compose installable legacy directory objects into one ext4 rootfs image

`Tree` is a direct authoring path:

- the builder accepts generated tree data in `config.tree`
- generated `*-tree.ncl` modules import UTF-8 text from the adjacent `*-tree-src`
  tree instead of embedding file contents inline
- it stages UTF-8 text files, symlinks, and explicit directories
- it publishes either one file object or one fs-tree directory object,
  depending on the tree shape

`Rootfs` and `Ext4Rootfs` are legacy direct composition paths:

- the builder reads installable legacy directory objects from the store
- it applies install rules from each input's `meta.install`
- it merges those filesystem contributions in memory
- `Rootfs` writes one staged directory object as the realized result
- `Ext4Rootfs` writes one ext4 image file directly as the realized result

`Rootfs` is the directory-producing counterpart to `Ext4Rootfs`.

`TreeMerge` is the manifest-based composition path for fs-tree objects:

- the builder reads canonical `manifest.jsonl` files from fs-tree inputs
- it validates each input `root/` directory against its manifest
- it merges manifest entries with strict conflict checking
- it writes a new fs-tree directory object
- regular files are hardlinked from input fs-trees when possible, with copy
  fallback for filesystems that do not support the hardlink
- symlinks are recreated with the same target

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
- otherwise the result is an fs-tree directory object:
  ```text
  manifest.jsonl
  root/
  ```
- `install` is rejected for file output and required for directory output
- `install.rules` uses path selectors with partial field overrides
- directory output consumes `install.rules` into `manifest.jsonl`
- directory output currently supports only logical `uid=0,gid=0`; any
  non-root `uid` or `gid` in `install.rules` is rejected until fs-tree owner
  materialization is implemented
- directory output publishes empty result metadata: `{}`
- `symlink_mode` is accepted in `install.rules` for config compatibility, but
  symlink modes are not represented in the fs-tree manifest
- authoring usually starts with one broad `**` rule carrying full defaults, then
  adds narrower overrides
- when authoring `*-tree-src`, empty directories must contain `.gitkeep`; the generator ignores `.gitkeep` and still emits an empty `dir` entry
- codegen staleness checks cover tree structure, symlink targets, and executable bits;
  text file contents are read at Nickel import time

Current limitations:

- tree entries currently support only UTF-8 text files, symlinks, and explicit directories
- binary files and richer file mode control are not yet supported

## `TreeMerge`

`TreeMerge` accepts this config:

```json
{}
```

Inputs:

- two or more named fs-tree directory inputs
- input order follows the standard builder input order: required inputs,
  optional inputs, then extra inputs in lexical input-name order

Current behavior:

- requires every input to be a valid fs-tree directory object:
  ```text
  manifest.jsonl
  root/
  ```
- reads canonical manifests and treats them as the source of truth
- validates each input `root/` directory against its manifest before merging
- allows overlapping directory paths only when `uid`, `gid`, and `mode` match
- rejects duplicate file or symlink paths
- rejects file-vs-directory, symlink-vs-directory, and parent/child leaf conflicts
- writes one fs-tree directory object with a canonical merged manifest

Physical materialization:

- directories are created as needed
- regular files are hardlinked from their source fs-tree when possible
- when hardlinking is not supported or not permitted, regular file bytes are copied
- symlinks are recreated with the same target
- ownership and modes are materialized and validated against the merged manifest

The realized result payload is one fs-tree directory object. The current realized
result metadata is empty:

- `{}`

## `Rootfs`

`Rootfs` accepts this config:

```json
{}
```

Inputs:

- one or more named installable legacy directory inputs
- contribution order follows lexical input name order

Current behavior:

- requires every input to resolve to a directory object
- requires every input to carry valid `meta.install.rules`
- does not yet accept fs-tree inputs from `Tree`
- scans files, directories, and symlinks from all inputs
- resolves install attributes per path using full coverage and field-wise last-match-wins semantics
- merges all filesystem contributions with the same strict conflict checking as `Ext4Rootfs`
- writes one staged directory object from the merged filesystem state

Physical materialization:

- directories are created as needed
- symlinks are recreated with the same target
- regular files are hardlinked from their source object when possible
- when hardlinking is not supported or not permitted, regular file bytes are copied
- target `uid`, `gid`, and mode values are not applied with `chown` or `chmod`
- the source executable bit remains the physical filesystem identity for regular files

The realized result payload is one directory object. The current realized result metadata is empty:

- `{}`

## `Ext4Rootfs`

`Ext4Rootfs` accepts this config:

```json
{
  "size_mib": 256,
  "label": "rootfs"
}
```

Inputs:

- one or more named installable legacy directory inputs
- contribution order follows lexical input name order

Current behavior:

- requires every input to resolve to a directory object
- requires every input to carry valid `meta.install.rules`
- does not yet accept fs-tree inputs from `Tree`
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

`Tree` fs-tree directory outputs currently support:

- regular files
- directories
- symlinks
- logical `uid=0,gid=0` ownership only

`Rootfs` and `Ext4Rootfs` currently support only filesystem content already supported by the legacy directory object path:

- regular files
- directories
- symlinks

Current limitations:

- special files such as block devices, character devices, FIFOs, and sockets are not supported
- the ext4 image size must be provided explicitly through `size_mib`
- `Rootfs` does not support a base input or replacement/overlay semantics
- `Rootfs` does not physically apply target ownership or modes
- the builder does not yet serve as the backend for `Image`; OCI-based composition remains a separate path
