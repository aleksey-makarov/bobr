# Rootfs Builders

## Summary

`bobr` currently implements filesystem-related builders around fs-tree
manifest:

- `Tree`: realize generated text files, symlinks, and directories as one plain
  file object or one ordinary directory object
- `FsTreeImport`: import one ordinary directory object into an fs-tree manifest
  object using install rules
- `TreeSubset`: select a subset of one fs-tree manifest object
- `TreeMerge`: merge two or more fs-tree manifest objects
- `OciExtract`: extract one OCI image layout into an fs-tree manifest object
- `ErofsRootfs`: build one EROFS image from one fs-tree input
- `Initramfs`: build one Linux `newc` initramfs image from one fs-tree input

An fs-tree manifest object is a normal store object whose payload is the
canonical manifest text. Regular file payloads referenced by that manifest live
in `<store>/fs-files/`. When a builder asks for a filesystem root rather than
the manifest file, the runtime materializes the manifest into the cache under
`<store>/fs-trees/<manifest-object-hash>/` and passes that root path to the
builder.

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
        "text": "bobr\n",
        "executable": false
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
- if the tree contains exactly one top-level file entry, the result is a file
  object
- otherwise the result is an ordinary directory object
- `install` is not accepted; use `FsTreeImport` to turn a directory object into
  an fs-tree manifest with logical ownership and mode metadata

Current limitations:

- tree entries currently support only UTF-8 text files, symlinks, and explicit
  directories
- binary files and richer file mode control are not yet supported

## `FsTreeImport`

`FsTreeImport` accepts this config:

```json
{
  "install": {
    "rules": [
      {
        "path": "**",
        "attrs": {
          "uid": 0,
          "gid": 0,
          "directory_mode": 493,
          "regular_file_mode": 420,
          "executable_file_mode": 493
        }
      }
    ]
  }
}
```

Inputs:

- required `input`: one ordinary file or directory object

Current behavior:

- imports the input directory into `<store>/fs-files/`
- writes one canonical fs-tree manifest object
- evaluates install rules in order; later matching rules override earlier
  attributes field-by-field
- directory, regular file, and executable file modes are represented by install
  attributes
- symlink mode is not represented by fs-tree manifest
- runs as a `bobr-runtime` namespace function because importing needs
  namespace-root access to ownership metadata

## `TreeMerge`

`TreeMerge` accepts this config:

```json
{}
```

Inputs:

- two or more named fs-tree manifest inputs
- input order follows the standard builder input order: required inputs,
  optional inputs, then extra inputs in lexical input-name order

Current behavior:

- reads canonical manifest inputs directly from their object paths
- merges manifest entries with strict conflict checking
- allows overlapping directory paths only when `uid`, `gid`, and `mode` match
- allows duplicate file paths only when the referenced fs-file hash matches
- allows duplicate symlink paths only when `uid`, `gid`, and target match
- rejects file-vs-directory, symlink-vs-directory, and parent/child leaf
  conflicts
- writes one canonical fs-tree manifest object

`TreeMerge` is manifest-only; it does not materialize the input trees.

## `TreeSubset`

`TreeSubset` accepts this config:

```json
{
  "include": [
    "usr/lib64/libfoo.so*",
    "usr/share/foo/**"
  ]
}
```

Inputs:

- required `tree`: one fs-tree manifest object

Current behavior:

- reads the canonical input manifest directly from its object path
- matches `include` globs against manifest paths
- rejects empty include lists, empty patterns, absolute patterns, and patterns
  containing `..`
- allows individual include patterns to match no paths
- rejects the build when the final selected subset contains no non-root paths
- includes matched files, symlinks, and directories plus their parent
  directories
- selecting a directory directly includes only that directory; recursive
  selection requires a pattern such as `dir/**`
- writes one canonical fs-tree manifest object

`TreeSubset` is manifest-only; it does not materialize the input tree.

## `ErofsRootfs`

`ErofsRootfs` accepts this config:

```json
{
  "compression": null,
  "label": null
}
```

Inputs:

- required `tree`: one fs-tree manifest object, materialized by the runtime
  before builder execution

Current behavior:

- receives a materialized filesystem root path for `tree`
- runs `mkfs.erofs` from `PATH` through a `bobr-runtime` namespace function:
  ```sh
  mkfs.erofs --sort=path -T 0 -U clear \
    [ -L label ] [ -z compression ] \
    rootfs.erofs <materialized-root>
  ```
- produces one regular file containing an EROFS filesystem image

Config fields:

- `compression = null` creates a plain EROFS image and does not pass `-z`
- non-null `compression` must be a non-empty string and is passed as
  `-z <compression>`
- non-null `label` must be a non-empty string and is passed as `-L <label>`

## `Initramfs`

`Initramfs` accepts this config:

```json
{}
```

Inputs:

- required `tree`: one fs-tree manifest object, materialized by the runtime
  before builder execution

Current behavior:

- receives a materialized filesystem root path for `tree`
- scans that root inside a `bobr-runtime` namespace function
- writes one deterministic Linux `newc` cpio archive directly, without
  invoking a host or target `cpio` program
- encodes the materialized root directory as `.`
- sets cpio directory and file `uid`, `gid`, `mode`, and `mtime=0` from the
  materialized filesystem metadata
- sets cpio symlink `uid`, `gid`, target payload, and `mtime=0`; symlink mode
  is encoded as `0777`
- terminates the archive with `TRAILER!!!`

The realized result payload is one regular file containing an uncompressed
initramfs archive suitable for Linux `-initrd` users such as QEMU.

## Current Limitations

Fs-tree manifest currently supports:

- regular files
- directories
- symlinks

Current limitations:

- special files such as block devices, character devices, FIFOs, and sockets
  are not supported
- xattrs, POSIX ACLs, file capabilities, and hardlink identity are not modeled
