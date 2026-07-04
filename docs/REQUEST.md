# Request

`bobr` reads a JSON document (described below) from standard input or from a
file named on the command line. The document is a **request**: it describes how
to build an object. `bobr` builds that object and prints its `ObjectHash` to
standard output. For the model behind requests — objects, recipes, keys — see
[Concepts](./CONCEPTS.md).

The request is a single JSON object:

```json
{
  "schema": "bobr-request-v1",
  "store": "/abs/path/to/store",
  "quiet": false,
  "jobs": 8,
  "nodes": {
    "root": { "...": "..." }
  }
}
```

- `schema` — format version; must be `"bobr-request-v1"`
- `store` — the store root for this request: an absolute path to an existing
  directory (see [Store](./STORE.md))
- `quiet` — optional bool; suppress the live progress log
- `jobs` — optional integer; limit on parallel builder execution
- `nodes` — the recipe DAG

The recipe DAG is a JSON object: each member's value is a recipe. The required
key `root` holds the recipe to build; the others hold the recipes it depends on.

A recipe for the `Source` builder has this shape:

```json
{
  "name": "linux-src",
  "tag": "Source",
  "object_hash": "0123…abcd",
  "origin": {
    "tag": "Http",
    "url": ["https://example.invalid/linux.tar.xz"],
    "unpack": true
  }
}
```

- `name` — a human-facing name for the result
- `tag` — must be `"Source"`
- `object_hash` — the `ObjectHash` this source must produce
- `origin` — how to obtain the object this recipe describes; defined below

A recipe for the `Source` builder may also omit `origin`. Then the object must
already exist in the store under its `object_hash`, and `bobr` reuses it; if it
does not, the source fails.

A recipe for any other builder has this shape:

```json
{
  "name": "tar-1.35",
  "tag": "Sandbox",
  "config": {
    "steps": [
      {
        "name": "build",
        "run_as": "build-user",
        "cwd": "@{build}",
        "argv": ["@{script}", "build"]
      }
    ]
  },
  "inputs": {
    "_rootfs": "rootfs_1",
    "script": "script_1",
    "source": "src_0"
  }
}
```

- `name` — a human-facing name for the result
- `tag` — the name of the builder that builds this recipe
- `config` — the builder's configuration; its shape is defined by the builder
- `inputs` — dependencies keyed by input name; each value is the key of another
  member of `nodes` — the recipe this one depends on

An input whose name begins with `_` is **materialized**: its object must be a
[filesystem tree](./FS_TREE.md) and the builder receives the path to a real
directory of files. Any other input is passed as the object itself (for an
fs-tree, that is the manifest object). This rule is uniform across builders and
applies to both the slots a builder declares and any extra inputs it accepts.

## Builders

### `Tree`

Realizes an inline description of text files, symlinks, and directories as one
object.

**Inputs:** none.

**Config:** a `tree` with a list of `entries`, each a `file`, `dir`, or
`symlink`:

```json
{
  "tree": {
    "entries": [
      { "type": "dir", "path": "etc" },
      { "type": "symlink", "path": "bin", "target": "usr/bin" },
      { "type": "file", "path": "etc/hostname", "text": "bobr\n", "executable": false }
    ]
  }
}
```

- a `file` entry carries UTF-8 `text` and an `executable` flag
- a `symlink` entry carries a literal `target` string
- a `dir` entry is an explicit directory

**Behavior:**

- parent directories for file entries are created automatically
- if the tree is exactly one top-level `file` entry, the result is a file
  object; otherwise it is an ordinary directory object
- `install` metadata is not accepted here — use [`FsTreeImport`](#fstreeimport)
  to attach logical ownership and mode

**Limitations:** only UTF-8 text files, symlinks, and explicit directories;
binary files and richer file-mode control are not yet supported.

### `FsTreeImport`

Imports one ordinary object into an fs-tree, applying install rules that set
logical ownership and mode.

**Inputs:** required `input` — one ordinary file or directory object.

**Config:** `install.rules`, evaluated in order; each rule is a glob `path` plus
`attrs` (any of `uid`, `gid`, `directory_mode`, `regular_file_mode`,
`executable_file_mode`):

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

**Behavior:**

- imports the input contents into the store's shared file storage and writes one
  fs-tree object
- later matching rules override earlier attributes field by field
- directory, regular-file, and executable-file modes come from install
  attributes; symlink mode is not represented
- runs as a namespace function, since importing needs namespace-root access to
  ownership metadata

### `FsTreeExport`

The inverse of `FsTreeImport`: extracts selected entries out of an fs-tree into
an ordinary object. Its `input` is an fs-tree, passed as a plain object (so the
builder receives the manifest and pulls matched files from shared storage by
content hash — it does **not** materialize the whole tree).

**Inputs:** required `input` — one fs-tree object.

**Config:** `copies`, a non-empty ordered array of `{ from, to }` commands:

```json
{ "copies": [ { "from": "boot/bzImage", "to": "bzImage" },
              { "from": "usr/lib/*.so.1", "to": "libs" } ] }
```

- `from` is a glob (or literal path) matched against the fs-tree's paths.
- `to` is the destination in the output object. For a **literal** `from` naming
  a single file or symlink, `to` is the exact output path (allowing rename). For
  a **glob** (or a literal directory), `to` is a directory and each match is
  placed under it preserving its path relative to the glob's literal base (for a
  literal directory, relative to that directory). Directory entries never copy
  on their own — parent directories are created as needed.
- A command that matches nothing, or two commands writing the same destination,
  is rejected.

**Behavior:**

- produces a plain-object directory: matched regular files (mode, including the
  executable bit, preserved), matched symlinks recreated, all owned `0:0`
- runs as a namespace function: reading arbitrary fs-files needs namespace-root
  (they carry their entries' logical ownership and mode), and a plain object
  must be single-owner

### `TreeMerge`

Merges two or more fs-trees into one, with strict conflict checking.

**Inputs:** two or more fs-tree inputs (as extra inputs), consumed in the
standard input order (required inputs, then optional, then extra inputs in
lexical name order).

**Config:** none (`{}`).

**Behavior:**

- reads the input manifests directly and does not materialize them
- overlapping directory paths are allowed only when `uid`, `gid`, and mode match
- duplicate file paths are allowed only when the referenced file content matches
- duplicate symlink paths are allowed only when `uid`, `gid`, and target match
- file-vs-directory, symlink-vs-directory, and parent/child leaf conflicts are
  rejected
- writes one fs-tree object

### `TreeSubset`

Produces an fs-tree containing only the paths that match its `include` patterns.

**Inputs:** required `tree` — one fs-tree.

**Config:** `include`, a non-empty list of glob patterns:

```json
{
  "include": [
    "usr/lib64/libfoo.so*",
    "usr/share/foo/**"
  ]
}
```

**Behavior:**

- reads the input manifest directly and does not materialize it
- matches `include` globs against manifest paths; a matched file, symlink, or
  directory is included together with its parent directories
- selecting a directory directly includes only that directory; recursive
  selection needs a pattern such as `dir/**`
- individual patterns may match nothing, but the build fails if the final subset
  has no non-root paths
- rejects empty include lists, empty patterns, absolute patterns, and patterns
  containing `..`
- writes one fs-tree object

### `OciExtract`

Extracts one OCI image layout into an fs-tree.

**Inputs:** required `image` — an OCI image layout object (for example, a
`Source`/`OciRegistry` result).

**Config:** none (`{}`).

**Behavior:**

- extracts the image root filesystem into one fs-tree object
- the result can be consumed as an fs-tree input by `TreeMerge`, `TreeSubset`,
  `Initramfs`, or `Sandbox`

### `Sandbox`

Runs an ordered plan of commands inside an isolated container — a set of Linux
namespaces rooted at the `_rootfs` input, with no network access — and captures
the `@{out}` directory as the result. By default the output is captured as an
ownership-aware fs-tree; see `preserve_ownership`.

**Inputs:**

- required `_rootfs` — one fs-tree; materialized (its name begins with `_`) and
  used as the container's read-only root filesystem
- any number of extra inputs — each made available to the steps through its
  interpolation name `@{name}` (read-only). An input name must start with an
  ASCII letter or `_`, contain only ASCII letters, digits, or `_`, and must not
  be `build`, `out`, or `config` (reserved). An extra whose name begins with `_`
  is materialized into a filesystem tree (see above) and `@{name}` is that
  directory; otherwise `@{name}` is the object itself.

**Config:**

- `steps` — a required, non-empty array of steps
- `script_config` — a config tree, available to the steps as `@{config}`
  (default `{}`, an empty config directory)
- `preserve_ownership` — whether the output is captured as an ownership-aware
  fs-tree (default `true`). When `false`, the `@{out}` tree is instead chowned to
  a single owner and captured as a plain object — for self-contained artifacts
  (e.g. a disk image) where per-file ownership is irrelevant

```json
{
  "script_config": {
    "configure_args": ["--disable-nls"]
  },
  "steps": [
    {
      "name": "build",
      "run_as": "build-user",
      "cwd": "@{build}",
      "argv": ["@{script}", "build"],
      "env": { "CC": "gcc" }
    }
  ]
}
```

Each step has:

- `name` — non-empty (after trimming); used in reports and log names
- `run_as` — `"build-user"` or `"root"`
- `cwd` — non-empty; must resolve to an absolute path
- `argv` — a non-empty array of non-empty strings
- `env` — optional object whose values are strings

Each step runs with a fixed default environment, which its `env` extends or
overrides: `PATH`, `HOME` (the build directory), `TMPDIR` (`/tmp`), `USER`
(`bobr`), `LC_ALL` and `LANG` (`C`), `TZ` (`UTC`), `SOURCE_DATE_EPOCH` (a fixed
epoch), and `PYTHONHASHSEED` (`0`) — locale, timezone, epoch, and hash seed are
pinned for reproducibility. The build, output, config, and inputs directories
are also exposed as `BOBR_BUILD_DIR`, `BOBR_OUT_DIR`, `BOBR_CONFIG_DIR`, and
`BOBR_INPUTS_DIR`, and the step's name as `BOBR_STEP_NAME`. `BOBR_BUILD_SEED`
carries a deterministic per-build seed (64 lowercase hex chars) for steps that
need a reproducible "random" value, such as a filesystem UUID; it is derived
from the build's reuse key, so identical inputs yield an identical seed.

`cwd`, each `argv` item, and each `env` value support `@{…}` interpolation
(`name`, `run_as`, `env` keys, and `script_config` do not):

- `@{build}` — the writable build directory
- `@{out}` — the writable output directory; its contents become the result
  object
- `@{config}` — the materialized `script_config` directory
- `@{<input>}` — an extra input: the materialized directory if its name begins
  with `_`, otherwise the read-only object path

`@@{name}` escapes to the literal `@{name}`. Unknown variables and malformed
interpolation are invalid config.

`script_config` may be absent or `{}` (an empty config directory); otherwise
it is a recursive tree: objects become directories, arrays become directories
with zero-padded numeric entries (`00000000`, …) in order, and strings become
file contents. Keys must be non-empty, must not be `.` or `..`, and may contain
only ASCII letters, digits, `.`, `_`, and `-`.

### `Initramfs`

Builds a deterministic Linux `newc` initramfs cpio archive from an fs-tree.

**Inputs:** required `_tree` — one fs-tree, materialized before execution.

**Config:** none (`{}`).

**Behavior:**

- scans the materialized root inside a namespace function and writes the `newc`
  cpio archive directly, without invoking any `cpio` program
- encodes the root as `.`; takes `uid`, `gid`, and mode from the materialized
  metadata with `mtime = 0`; symlink mode is encoded as `0777`; the archive ends
  with `TRAILER!!!`
- the result is one regular file — an uncompressed initramfs suitable for Linux
  `-initrd` users such as QEMU

### `Group`

Aggregates several otherwise unrelated targets under one `root`.

**Inputs:** one or more extra inputs (arbitrary).

**Config:** none (`{}`).

**Behavior:** does not merge or inspect its inputs; it stages a constant
zero-byte marker once all inputs are realized, so its object is only a
completion marker — the meaningful results are the input targets themselves.

## Origins

A recipe for the `Source` builder obtains its object from an `origin`, one of:

- **`Path`** — `origin.path` is an absolute host path; `origin.unpack` (default
  `false`) treats it as a tar archive when true.
- **`Http`** — `origin.url` is one HTTP(S) URL or an ordered list of fallbacks;
  `origin.unpack` (default `false`); `origin.archive_format` may override archive
  detection for unpacked sources.
- **`OciRegistry`** — `origin.image` is the registry image locator,
  `origin.digest` the pinned `sha256:` manifest or index digest, and
  `origin.platform` (`{ "os": …, "architecture": … }`) selects the platform to
  pull. `bobr` fetches the pinned manifest (selecting the platform from a
  manifest list or index), downloads and verifies every blob, and writes an OCI
  image layout whose canonical form is independent of the registry mirror named
  by `origin.image`.
