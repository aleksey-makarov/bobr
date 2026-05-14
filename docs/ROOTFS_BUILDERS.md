# Rootfs Builders

## Summary

`mbuild` currently implements three filesystem-related builders:

- `Tree`: realize text files, symlinks, and explicit directories as one file object or
  one fs-tree directory object
- `TreeMerge`: compose fs-tree directory objects into one fs-tree directory object
- `ErofsRootfs`: compose fs-tree directory objects into one EROFS rootfs image

`Tree` is a direct authoring path:

- the builder accepts generated tree data in `config.tree`
- generated `*-tree.ncl` modules import UTF-8 text from the adjacent `*-tree-src`
  tree instead of embedding file contents inline
- it stages UTF-8 text files, symlinks, and explicit directories
- it publishes either one file object or one fs-tree directory object,
  depending on the tree shape

`TreeMerge` is the manifest-based composition path for fs-tree objects:

- the builder reads canonical `manifest.jsonl` files from fs-tree inputs
- it validates each input `root/` directory against its manifest
- it merges manifest entries with strict conflict checking
- it writes a new fs-tree directory object
- regular files are hardlinked from input fs-trees when possible, with copy
  fallback for filesystems that do not support the hardlink
- symlinks are recreated with the same target

`ErofsRootfs` is the image-producing counterpart to `TreeMerge`:

- the builder reads canonical `manifest.jsonl` files from fs-tree inputs
- it merges manifest entries with the same conflict semantics as `TreeMerge`
- it writes a deterministic tar stream from the merged manifest
- regular file bytes are read from the selected input fs-tree roots inside the
  ownership user namespace
- tar headers use logical `uid`, `gid`, mode, symlink targets, and `mtime=0`
  from the merged manifest
- it runs `mkfs.erofs --tar=f` on the host to produce one EROFS image file

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

## `ErofsRootfs`

`ErofsRootfs` accepts this config:

```json
{
  "compression": null,
  "label": null
}
```

Inputs:

- one or more named fs-tree directory inputs
- input order follows the standard builder input order: required inputs,
  optional inputs, then extra inputs in lexical input-name order

Current behavior:

- requires every input to have fs-tree object shape:
  ```text
  manifest.jsonl
  root/
  ```
- reads canonical manifests and treats them as the source of truth
- allows overlapping directory paths only when `uid`, `gid`, and `mode` match
- rejects duplicate file or symlink paths
- rejects file-vs-directory, symlink-vs-directory, and parent/child leaf conflicts
- writes a deterministic tar stream in canonical manifest order, excluding the
  implicit root entry
- sets tar directory and file `uid`, `gid`, `mode`, and `mtime=0` from the
  merged manifest
- sets tar symlink `uid`, `gid`, target, and `mtime=0` from the merged manifest;
  symlink mode is encoded as `0777` because fs-tree manifests do not carry
  symlink mode
- reads file bytes from input `root/` directories inside the ownership user
  namespace, so files owned through the configured idmap remain readable
- runs `mkfs.erofs` from `PATH` on the host:
  ```sh
  mkfs.erofs --tar=f --sort=path -T 0 -U clear \
    [ -L label ] [ -z compression ] \
    rootfs.erofs rootfs.tar
  ```

Config fields:

- `compression = null` creates a plain EROFS image and does not pass `-z`
- non-null `compression` must be a non-empty string and is passed as
  `-z <compression>`
- non-null `label` must be a non-empty string and is passed as `-L <label>`

The realized result payload is one regular file containing an EROFS filesystem
image. The current realized result metadata is empty:

- `{}`

## Current Limitations

`Tree` fs-tree directory outputs currently support:

- regular files
- directories
- symlinks
- logical `uid=0,gid=0` ownership only

Current limitations:

- special files such as block devices, character devices, FIFOs, and sockets are not supported
- the builder does not yet serve as the backend for `Image`; OCI-based composition remains a separate path
