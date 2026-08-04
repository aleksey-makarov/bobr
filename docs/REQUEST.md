# Request

`bobr` reads a JSON document (described below) from standard input or from a
file named on the command line. The document is a **request**: it describes how
to build an object. `bobr` builds that object and prints its `ObjectHash` to
standard output. For the model behind requests — objects, recipes, keys — see
[Concepts](./CONCEPTS.md).

The request is a single JSON object:

```json
{
  "schema": "bobr-request-v2",
  "store": "/abs/path/to/store",
  "logs": "/abs/path/to/logs/260803120000",
  "work": "/abs/path/to/work/260803120000",
  "run_id": "260803120000",
  "quiet": false,
  "jobs": 8,
  "nodes": {
    "root": { "...": "..." }
  }
}
```

- `schema` — format version; must be `"bobr-request-v2"`. `bobr --version`
  reports the schema this build accepts (`bobr 0.1.5 (request
  bobr-request-v2)`), so a recipe layer can check compatibility before building
  rather than finding out from the parse error
- `store` — the store root for this request: an absolute path to an existing
  directory (see [Store](./STORE.md))
- `logs` — this run's log directory: an absolute path to an existing directory,
  which `bobr` fills with the run log and one subdirectory per subject (see
  [Build logging](./LOGGING.md))
- `work` — this run's work directory: an absolute path to an existing directory,
  which `bobr` fills with one scratch directory per subject
- `run_id` — what to call this run; recorded in the object records it writes and
  in each subject's `meta.json`. It must start with an ASCII letter or digit and
  may contain only ASCII letters, digits, `.`, `_`, and `-`, up to 64 characters
- `quiet` — optional bool; suppress the live progress log
- `jobs` — optional integer; limit on parallel builder execution
- `nodes` — the recipe DAG

`bobr` neither names the run nor creates its two directories: the caller does
both, which is also what keeps two runs from writing into one another. `bobr`
writes into what it is given, and refuses to share it: the run's `events.jsonl`
is created exclusively, so a second run pointed at a log directory already in use
(or left by an earlier run) fails immediately instead of interleaving with it.

The **work directory must be on the same filesystem as the store**. Builders
stage their output there, and the store publishes it by renaming it into
`objects/` and hardlinking its files into `fs-files/`; neither crosses a
filesystem boundary. `bobr` checks this before building rather than failing on
the first import. The log directory has no such constraint.

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

### `Bundle`

Collects an arbitrary number of file inputs into one directory object.

**Inputs:** one or more file inputs (as extra inputs). Each input must be a
regular file (not a directory or symlink).

**Config:** none (`{}`).

**Behavior:**

- writes a plain directory object with one entry per input, a hardlink named by
  the input's own name (inputs are hardlinked, never copied)
- the result is ordinary data with no ownership or mode identity — unlike the
  fs-tree builders; use it to hand a downstream build many inputs as a single
  directory (one bind mount) instead of one bind mount per input, e.g. to bundle
  vendored crate archives

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

### `TreeMove`

Re-roots an fs-tree at one of its subdirectories: the `strip_prefix` directory
becomes the new root and its leading path component is dropped from every nested
entry. A pure manifest operation — the same fs-files are referenced under shorter
paths.

**Inputs:** required `tree` — one fs-tree.

**Config:** `strip_prefix`, the subdirectory to promote to the root:

```json
{ "strip_prefix": "stage" }
```

**Behavior:**

- reads the input manifest directly and does not materialize it
- promotes the `strip_prefix` subtree to the root — each entry keeps its content
  and metadata, with the leading `strip_prefix` component removed — and writes
  one fs-tree object

### `HostBundle`

Builds a verified, relocatable host-side application directory from an already
materialized runtime root and a static launcher package. See
[HostBundle](./HOST_BUNDLE.md) for the complete composition, verification, and
runtime model.

**Inputs:**

- required `_root` — the materialized payload fs-tree
- required `_launcher` — a materialized fs-tree containing
  `usr/libexec/bobr-bundle-launcher`
- optional `overrides` — an ordinary directory object copied under the
  bundle's `overrides/`

**Config:**

```json
{
  "arch": "x86_64",
  "policy": "strict",
  "min_kernel": "4.19",
  "library_dirs": ["usr/lib64", "usr/lib"],
  "public_tools": {
    "demo": {
      "path": "usr/bin/demo",
      "argv0": "demo",
      "argument_prefix": [
        { "value": "--data-dir" },
        { "source": "payload", "path": "usr/share/demo" }
      ],
      "environment": {
        "DEMO_CONFIG": {
          "operation": "replace",
          "paths": [
            { "source": "payload", "path": "etc/demo/config.toml" }
          ]
        }
      }
    }
  },
  "internal_tools": {
    "demo-helper": { "path": "usr/libexec/demo/helper" }
  },
  "environment": {
    "LOCALE_ARCHIVE": {
      "operation": "replace",
      "paths": [
        { "source": "payload", "path": "usr/lib/locale/locale-archive" }
      ]
    }
  }
}
```

Top-level fields:

- required `arch` — `"x86_64"` or `"aarch64"`
- optional `policy` — `"strict"` (the default) or `"integrated"`; the current
  runtime records and reports this value, but both values use the same launch
  and verification rules
- optional `min_kernel` — `MAJOR.MINOR` or `MAJOR.MINOR.PATCH`, default `4.19`
- required `library_dirs` — ordered safe paths relative to `_root`; it may be
  empty only for a completely static configured startup closure
- required non-empty `public_tools` — commands exposed in top-level `bin/`
- optional `internal_tools` — commands exposed only through the managed child
  process `PATH`
- optional `environment` — rules shared by all tools

A tool declaration requires `path`, a safe path relative to `_root` resolving
to an executable regular file. Optional `argv0` defaults to the tool name.
Optional `argument_prefix` inserts entries before caller arguments; each entry
is either a literal `{ "value": "..." }` or a path selected from `payload` or
`overrides`. Optional per-tool `environment` is applied after the common rules.

Tool names are non-empty UTF-8 basenames: `.`, `..`, names containing `/` or
NUL, and the reserved `bobr-bundle-launcher` name are invalid. A name cannot be
both public and internal. `argv0` must be non-empty and contain no NUL.

An environment rule has this shape:

```json
{
  "operation": "prepend",
  "paths": [
    { "source": "payload", "path": "usr/share/example" }
  ],
  "values": [],
  "inherit": true,
  "host_default": ["/fallback"]
}
```

`paths`, `values`, `inherit`, and `host_default` are optional and default to an
empty array or `false`. `paths` and `values` are mutually exclusive; multiple
entries are joined with `:`. Operations are:

- `replace` — require configured paths or values and ignore the host;
  `inherit` and `host_default` are invalid
- `prepend` / `append` — require configured paths or values; include the
  current value only with `inherit = true`, using `host_default` when that
  value is absent
- `unset` — remove the variable and accept none of the other fields
- `default` — preserve an existing variable, otherwise use configured paths or
  values, or `host_default`; `inherit` is invalid

For `prepend` and `append`, `host_default` requires `inherit = true`. Variable
names must be non-empty and contain neither `=` nor NUL. `PATH` is reserved in
both common and per-tool rules: the builder always prepends
`libexec/wrapped-bin` to the inherited host `PATH`.

A typed path has source `"payload"` or `"overrides"` and a safe path relative
to that source. Paths must be non-empty and relative, with no NUL and no empty,
`.` or `..` component. Referencing `overrides` requires the optional input to
be present. Paths are canonicalized and must remain inside the corresponding
completed bundle tree. The top-level config, tool declarations, literal
arguments, and environment rules reject unknown fields.

**Behavior:**

- independently copies the payload to `root/`, the launcher to `libexec/`, and
  optional overrides to `overrides/`; it creates no hardlinks to input/store
  objects
- writes a read-only ordinary directory object with public wrappers in `bin/`
  and wrappers for every public/internal tool in `libexec/wrapped-bin/`
- generates the versioned `bobr-host-bundle-v2` `bundle.toml` and records the
  selected architecture as `[platform].arch`
- verifies that the launcher, tools, dynamic loaders, and complete startup
  `DT_NEEDED` closure are ELF64 objects for the declared architecture; scripts
  and their bundled shebang interpreters are verified recursively
- resolves startup libraries only inside the payload, accepting safe
  `$ORIGIN` RPATH/RUNPATH entries and rejecting unsupported or escaping paths
- removes write, setuid, and setgid bits before publication
- never executes target code while composing the bundle, so the builder itself
  is host-architecture-independent; the explicit config and input hashes carry
  all target-architecture differences into the build key

### `OciExtract`

Extracts one OCI image layout into an fs-tree.

**Inputs:** required `image` — an OCI image layout object (for example, a
`Source`/`OciRegistry` result).

**Config:** none (`{}`).

**Behavior:**

- extracts the image root filesystem into one fs-tree object
- the result can be consumed as an fs-tree input by `TreeMerge`, `TreeMove`,
  `TreeSubset`, `Sandbox`, or `SandboxInstall`

### `Sandbox`

Runs an ordered plan of commands on a read-write overlay root — a set of Linux
namespaces with no network access, over the materialized `_rootfs` — and captures
the `@{out}` staging directory as the result: chowned to a single owner and
stored as a plain object. Changes the steps make to the root itself are
discarded; only `@{out}` is kept. To keep those changes instead, see
[`SandboxInstall`](#sandboxinstall).

**Inputs:**

- required `_rootfs` — one fs-tree; materialized (its name begins with `_`) and
  used as the read-only lower layer of the build's read-write overlay root
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
  object (`Sandbox` only)
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

### `SandboxInstall`

The additive counterpart of `Sandbox`: the same overlay run — identical
`_rootfs`, extra inputs, `steps`, `script_config`, environment, and `@{…}`
interpolation — but instead of a separate `@{out}` it captures the changes the
steps make to the root as an **additive fs-tree layer**: the overlay's upper
layer, an ownership-aware delta over `_rootfs` meant to be `TreeMerge`d back onto
it.

**Inputs:** required `_rootfs` plus any number of extra inputs — as
[`Sandbox`](#sandbox).

**Config:** `steps` and `script_config` — as [`Sandbox`](#sandbox).

**Behavior:**

- runs the steps on the same read-write overlay root, then writes one fs-tree
  object from the upper layer (created and modified entries, pruned of pure
  copy-ups)
- there is no `@{out}` / `BOBR_OUT_DIR`; using `@{out}` is an unknown-variable
  error — the result is the delta, not a staging directory

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
