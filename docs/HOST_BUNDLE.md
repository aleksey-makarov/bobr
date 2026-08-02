# HostBundle

A HostBundle is a verified, relocatable directory containing a host-side
application and the userspace it needs. It can run directly from the bobr store
or after the complete directory has been copied elsewhere. It does not require
bobr, Nix, root privileges, a container runtime, a mount namespace, or a fixed
installation prefix on the machine where it runs.

The bundle is not a container and does not provide a virtual `/`. The process
still sees the host kernel, filesystem, home directory, devices, display and
audio sockets, network, and system services. HostBundle controls how declared
commands enter the copied userspace; it does not isolate those commands from the
host.

## Model

A normal dynamically linked ELF names an absolute interpreter in `PT_INTERP`,
for example `/lib64/ld-linux-x86-64.so.2`. Executing that ELF directly asks the
host kernel to open the host path. That fails on systems without a conventional
FHS loader path, such as NixOS, and can select a loader from an unrelated
userspace on other distributions.

A HostBundle instead starts through a small statically linked musl executable,
`bobr-bundle-launcher`. The launcher finds the bundle, reads the target ELF's
`PT_INTERP`, resolves that path below the copied payload, and invokes the
bundled glibc loader explicitly:

```text
<bundle>/root/lib64/ld-linux-x86-64.so.2 \
    --inhibit-cache \
    --argv0 demo \
    --library-path <bundle>/root/usr/lib64:<bundle>/root/usr/lib \
    <bundle>/root/usr/bin/demo ...
```

The launcher itself has neither `PT_INTERP` nor `DT_NEEDED`, so it can start
without using the host C library. The builder proves the declared startup
closure using files under `root/` before publishing the bundle.

The current implementation supports Linux/glibc payloads for `x86_64` and
`aarch64`. A bundle is architecture-specific. It also declares a minimum Linux
kernel version, which defaults to `4.19`.

## Constructing a bundle in Nickel

HostBundle is a real bobr builder, not a synthetic Nickel builder. A recipe
normally constructs the payload with `RootfsClosure`, selects a launcher package
for the same architecture, and passes both materialized trees to `HostBundle`:

```nickel
fun pkgs =>
  let payload = {
    name = "demo-host-bundle-payload",
    tag = "RootfsClosure",
    config = {},
    inputs = {
      demo = pkgs.demo,
      locales = pkgs.glibc_locales,
    },
  }
  in

  let demo_host_bundle = {
    name = "demo-host-bundle",
    tag = "HostBundle",
    config = {
      arch = "x86_64",
      policy = "strict",
      min_kernel = "4.19",
      library_dirs = ["usr/lib64", "usr/lib"],

      public_tools = {
        demo = {
          path = "usr/bin/demo",
          argument_prefix = [
            { value = "--data-dir" },
            { source = "payload", path = "usr/share/demo" },
          ],
          environment = {},
        },
      },

      internal_tools = {
        helper = {
          path = "usr/libexec/demo/helper",
        },
      },

      environment = {
        LOCALE_ARCHIVE = {
          operation = "replace",
          paths = [
            {
              source = "payload",
              path = "usr/lib/locale/locale-archive",
            },
          ],
        },
      },
    },
    inputs = {
      _root = payload,
      _launcher = pkgs.bobr_bundle_launcher_host_x86_64,
    },
  }
  in

  { include [demo_host_bundle] }
```

`RootfsClosure` is useful here because HostBundle does not calculate package
dependencies. The recipe layer supplies an already assembled runtime tree; the
builder copies and verifies it. A narrower manually composed fs-tree is also
valid if it contains everything the application needs.

### Inputs

The builder accepts exactly these inputs:

- `_root` is a required materialized fs-tree. It becomes the payload under
  `root/`.
- `_launcher` is a required materialized fs-tree containing
  `usr/libexec/bobr-bundle-launcher`. Only that regular file is copied from the
  launcher package.
- `overrides` is an optional ordinary directory object. It is copied under
  `overrides/` without first converting it to an fs-tree.

The leading underscores on `_root` and `_launcher` use the normal request rule
for materialized inputs. `overrides` deliberately has no underscore: it is a
plain directory object. All three inputs are treated as read-only. The builder
does not modify a materialized fs-tree, its fs-files, or an overrides object.

The launcher must be a static executable for the configured architecture. A
matching launcher should be built or selected explicitly; the builder never
compiles one and never substitutes a launcher from the build host.

### Top-level configuration

The complete build-time configuration has these fields:

- `arch` is required and is either `"x86_64"` or `"aarch64"`.
- `policy` is optional, defaults to `"strict"`, and also accepts
  `"integrated"`.
- `min_kernel` is optional and defaults to `"4.19"`. It must have the form
  `MAJOR.MINOR` or `MAJOR.MINOR.PATCH`, with decimal components.
- `library_dirs` is a required ordered array of paths relative to `_root`.
- `public_tools` is a required non-empty record of commands exposed in the
  top-level `bin/`.
- `internal_tools` is an optional record of commands exposed only through the
  managed child-process `PATH`.
- `environment` is an optional record of rules applied to every tool.

The top-level config, tool declarations, literal arguments, and environment
rules are strict typed records. The builder owns the generated `bundle.toml`
format version; a recipe cannot select or override it.

`policy` is currently recorded in `bundle.toml` and diagnostics, but `strict`
and `integrated` do not yet select different runtime algorithms. Both use the
same path validation, loader-sensitive environment filtering, bundled loader,
and startup verifier. Recipes must express intentional host integration through
explicit environment rules and tool arguments rather than relying on the
policy name to permit a fallback.

### Tool declarations

Every entry in `public_tools` and `internal_tools` has this shape:

```nickel
demo = {
  path = "usr/bin/tool",
  argv0 = "tool",
  argument_prefix = [
    { value = "--flag" },
    { source = "payload", path = "usr/share/tool" },
    { source = "overrides", path = "tool/config.json" },
  ],
  environment = {},
}
```

Only `path` is required:

- `path` is relative to `_root`. It must resolve to an executable regular file
  inside the copied `root/`.
- `argv0` is the logical process name. It defaults to the tool's record key and
  must be non-empty and contain no NUL.
- `argument_prefix` is an optional ordered array inserted before arguments from
  the caller.
- `environment` is an optional record applied after the common environment.

A tool name is a non-empty UTF-8 basename. Empty names, `.`, `..`, names
containing `/` or NUL, and the reserved name `bobr-bundle-launcher` are
rejected. The same name cannot appear in both public and internal records.

A fixed argument is one of:

- `{ value = "..." }`, a literal UTF-8 argument;
- `{ source = "payload", path = "..." }`, resolved below `root/`;
- `{ source = "overrides", path = "..." }`, resolved below `overrides/`.

Path arguments are canonicalized when the launcher runs and must remain inside
the bundle. An overrides path is invalid unless the recipe supplies the
`overrides` input. Fixed arguments precede all caller-provided arguments.

### Safe relative paths

Tool paths, library directories, environment paths, and fixed path arguments
are UTF-8 paths relative to their declared source. They must be non-empty, must
not begin with `/`, contain NUL, or contain an empty, `.` or `..` component.
The builder also canonicalizes paths in the completed directory and rejects
symlinks that escape the required root.

Paths used in colon-separated environment values or `--library-path` cannot
contain `:` after resolution. Spaces and Unicode are supported.

## Environment rules

The launcher starts with the inherited host environment, removes every
variable whose name begins with `LD_`, and removes `GLIBC_TUNABLES`. It then
applies common rules followed by the selected tool's rules. An explicitly
configured rule can set a name that was removed; inherited loader-sensitive
values never pass through implicitly.

Each rule contains:

```nickel
VARIABLE = {
  operation = "replace",
  paths = [{ source = "payload", path = "usr/share/example" }],
  values = [],
  inherit = false,
  host_default = [],
}
```

`operation` is required. The other fields default to an empty array or `false`.
`paths` and `values` are mutually exclusive. Multiple entries are joined with
`:`; scalar variables therefore normally use one `value`.

The operations are:

- `replace`: set the variable to the configured `paths` or `values`. At least
  one entry is required; `inherit` and `host_default` are forbidden.
- `prepend`: place the configured value before the current value. The current
  value participates only when `inherit = true`. If it is absent,
  `host_default` is used when provided.
- `append`: the same as `prepend`, but place the configured value after the
  current value.
- `unset`: remove the variable. It accepts no paths, values, inheritance, or
  defaults.
- `default`: leave an existing variable unchanged; otherwise set it from the
  configured entries, or from `host_default` when no entries are configured.
  `inherit` is forbidden.

For `prepend` and `append`, `host_default` requires `inherit = true`. Variable
names must be non-empty and contain neither `=` nor NUL.

`PATH` is reserved in both common and per-tool environment records. The builder
always generates this rule:

```text
prepend <bundle>/libexec/wrapped-bin to inherited PATH
```

This fixed rule keeps managed wrappers ahead of host commands. A recipe cannot
replace it accidentally.

## Output layout

The result is one ordinary directory object:

```text
<bundle>/
├── bin/
│   └── demo -> ../libexec/bobr-bundle-launcher
├── libexec/
│   ├── bobr-bundle-launcher
│   └── wrapped-bin/
│       ├── demo   -> ../bobr-bundle-launcher
│       └── helper -> ../bobr-bundle-launcher
├── bundle.toml
├── overrides/                 # present only when supplied
└── root/
    └── ...                    # independent copy of _root
```

`bin/` contains one symlink for every public tool and no internal-only tools.
`libexec/wrapped-bin/` contains every public and internal tool. The physical
payload files retain their original names and locations below `root/`; the
builder does not append `.real`, rewrite ELF files, patch shebangs, or modify
configuration files in the payload.

The generated `bundle.toml` is the runtime representation consumed by the
launcher. Its current format identifier is `bobr-host-bundle-v2`. It contains
bundle-relative paths after the builder has lowered the recipe's `payload` and
`overrides` namespaces. It is an internal producer/consumer format, not a file
that users are expected to author or edit.

## Builder execution

The HostBundle builder performs these phases in order.

### 1. Validate and lower the declaration

The builder validates the typed config before touching the output tree. It
maps payload paths to `root/...`, overrides paths to `overrides/...`, inserts
the mandatory `PATH` rule, combines public and internal tools, and constructs
the same typed runtime model used by the launcher.

The target architecture is configuration, not a property of the build host.
The builder parses target files but does not execute them, so an AArch64 host
can compose an x86-64 bundle and an x86-64 host can compose an AArch64 bundle
when matching input trees already exist.

### 2. Copy inputs independently

The builder creates a new staging directory and recursively copies `_root`, the
single launcher file, and optional overrides. It recreates symlinks without
following them, visits directory entries deterministically, preserves the
copied mode initially, and normalizes ownership to the owner of the output
object. Sockets, devices, FIFOs, and other special files are rejected.

Regular files are copied into new inodes. The result has no hardlinks to
materialized fs-trees, fs-files, launcher inputs, or overrides inputs. Hardlinks
within an input are not currently preserved: each path becomes an independent
file in the output.

### 3. Generate the facade

The builder creates the public and internal launcher symlinks and serializes
`bundle.toml`. Every public tool also receives an internal wrapper, allowing a
program to invoke itself or another public command by name through the managed
`PATH`.

### 4. Verify structure and startup closure

Structural verification checks the launcher, runtime config, wrapper targets,
tool paths, library directories, and configured environment paths in the
staged directory. Canonical paths must remain inside the expected bundle or
payload root.

The startup verifier then walks every public and internal executable without
running target code. A failure aborts the builder; an unverified HostBundle is
never published as a successful result.

### 5. Finalize read-only permissions

After every generated file and verification pass is complete, the builder
removes all write, setuid, and setgid bits from every non-symlink path. Read and
execute bits are otherwise preserved. It walks the result again to verify this
invariant before returning the ordinary directory object.

Read-only modes prevent accidental writes by normal execution. They are not a
security boundary against the owner of the object, who can deliberately change
permissions outside the normal store workflow.

## Startup verification

The verifier proves the initial ELF and script dependency graph using only the
staged bundle. It never asks the host dynamic loader, `ld.so.cache`, `/lib`, or
`/usr/lib` to resolve a dependency.

### ELF executables and libraries

All inspected ELF files must be 64-bit, little-endian, and have the `e_machine`
value selected by `arch`. Configured tools may be static or dynamic.

For a dynamic executable the verifier:

- reads its absolute `PT_INTERP` and resolves the corresponding executable
  below `root/`;
- requires the bundled loader to have neither its own `PT_INTERP` nor
  `DT_NEEDED`;
- reads `DT_NEEDED`, `DT_RPATH`, `DT_RUNPATH`, `SONAME`, GNU version needs, and
  GNU version definitions;
- resolves the complete transitive startup closure;
- rejects a `DT_NEEDED` containing `/`;
- checks a dependency's `SONAME` when it declares one;
- verifies that each required symbol version is provided by the selected
  dependency.

`library_dirs` is the ordered equivalent of the launcher's explicit
`--library-path`. Each entry must exist as a directory inside `root/`. The
array may be empty only when the complete configured startup closure is static.

The modeled search order follows glibc's relevant startup rules:

1. an object's `DT_RPATH`, when that object has no `DT_RUNPATH`, followed by
   inherited RPATH entries;
2. configured `library_dirs`;
3. that object's `DT_RUNPATH`.

The verifier accepts only `$ORIGIN` and `${ORIGIN}` entries whose result stays
inside the payload. Absolute paths, plain relative entries, `$LIB`,
`$PLATFORM`, other substitutions, and unsafe combinations are rejected. The
launcher passes `--inhibit-cache`, and the verified startup closure ensures the
loader does not need to reach its standard host fallback directories for the
declared initial dependencies.

This proof covers startup `DT_NEEDED`, not every later `dlopen` or application
plugin lookup. Those require explicit recipe configuration and runtime tests.

### Scripts

A configured script must have a safe absolute shebang interpreter. The path is
resolved below `root/`, must be an executable regular file, and is verified
recursively. The launcher supports at most four script nodes in one dispatch
chain, matching the builder's limit.

The verifier specially accepts:

```text
#!/usr/bin/env command
```

only when `command` is one public or internal tool in the same bundle. The
argument must be exactly one command name: options, paths, whitespace, and
`env -S` are currently rejected. At runtime the bundled `env` finds the managed
wrapper first in `PATH`.

Other shebang interpreters may receive the single optional argument represented
by the kernel shebang format. The interpreter and any recursive interpreter are
entered using the same static-or-bundled-loader dispatch as a normal tool.

## Launcher execution

The real launcher must be at
`<bundle>/libexec/bobr-bundle-launcher`. It resolves `/proc/self/exe`, verifies
that filename and parent directory, and derives the bundle root by moving one
directory upward. Consequently normal operation requires a mounted procfs. The
algorithm does not depend on the caller's working directory or on the symlink
path used to enter the launcher.

### Invocation

Running a facade symlink selects a tool by the basename of `argv[0]`:

```sh
./bin/demo argument
```

The real launcher also has an explicit interface:

```sh
./libexec/bobr-bundle-launcher --run demo -- argument
./libexec/bobr-bundle-launcher --diagnose demo
./libexec/bobr-bundle-launcher --diagnose demo --json
```

The `--` separator is required for `--run`. Payload arguments are preserved as
opaque Unix strings and follow the configured fixed argument prefix.

Before execution the launcher checks that the running OS is Linux, its compiled
architecture matches the bundle, and `uname(2)` reports at least `min_kernel`.
Normal execution fails on a mismatch. Diagnostic mode reports the mismatch but
continues preparing the report.

### Static, dynamic, and script dispatch

A static ELF, including a static position-independent executable without
`PT_INTERP`, is passed directly to `execve`.

For a dynamic ELF the launcher canonicalizes the payload interpreter and
library directories, verifies their containment and architecture, and replaces
itself with the bundled glibc loader. The loader receives, in order:

- `--inhibit-cache`;
- `--argv0` and the configured logical name;
- `--library-path` and the absolute resolved library directories;
- the physical target under `root/`;
- fixed arguments, then caller arguments.

Scripts are reduced recursively to a final static or dynamic interpreter plan.
In every successful case the launcher replaces itself rather than supervising
a child process. Signals, wait status, and exit status therefore belong directly
to the payload program or bundled loader.

## Public commands and child processes

The separate `bin/` and `libexec/wrapped-bin/` directories serve different
interfaces:

- `bin/` is the public interface users add to `PATH` or invoke directly;
- `wrapped-bin/` is the private interface inherited by bundled processes.

If `demo` runs `execvp("helper", ...)`, invokes `helper` through a shell, or
uses `/usr/bin/env helper`, `wrapped-bin/helper` enters the launcher again. The
helper therefore receives the same bundled loader and environment policy.

Putting internal wrappers in the public `bin/` would expose implementation
details and could shadow host commands for users of the bundle. Conversely,
putting only public tools in the managed path would let internal helpers bypass
the launcher.

Wrappers cannot intercept a literal `execve("/usr/bin/helper", ...)` or a
program started by its physical path below `root/`. Such calls follow normal
host absolute-path semantics or ask the host kernel to interpret the payload
ELF. A required child must be declared as a tool and invoked by name, or the
package must be configured to use the wrapper-compatible path.

## Overrides and application data

`root/` is intentionally an unchanged copy of the supplied rootfs structure.
When an application needs bundle-location-specific configuration, the recipe
should create an ordinary directory object and pass it as `overrides`.

For example, a bundle can place a generated fontconfig file at
`overrides/fontconfig/fonts.conf` and set `FONTCONFIG_FILE` with an overrides
environment path. QEMU disk, kernel, and initramfs files can similarly live
under `overrides/qemu/` and be selected through typed path variables. This
keeps adaptations explicit without patching the package payload.

The builder does not generate arbitrary application configuration and does not
infer plugin or data paths. The recipe produces those files and declares how a
tool finds them.

## Diagnostics

`--diagnose` resolves the same config, paths, environment, executable format,
and launch plan as normal execution, but does not call `execve`. Human output is
stable line-oriented text; `--json` emits a structured report.

The report includes:

- bundle and payload roots;
- configured policy;
- required architecture and minimum kernel, the host kernel, and compatibility;
- tool name, visibility, target, and resolved fixed arguments;
- ELF or script format, static/dynamic linkage, `PT_INTERP`, selected loader,
  and library path;
- the final environment and whether each value came from the host, a common
  rule, or a per-tool rule.

Diagnostics describe the prepared launch. They do not execute an application
probe, enumerate the build-time `DT_NEEDED` graph, or validate devices and
session sockets.

## Running and copying

The object can be invoked directly from a store ref or hash:

```sh
<store>/object-refs/demo-host-bundle/bin/demo
<store>/objects/<hash>/bin/demo
```

No export is needed for use on the same machine. To move a bundle, copy the
complete directory while preserving symlinks and executable modes, for example:

```sh
cp -a <store>/objects/<hash> /destination/demo-host-bundle
```

The copied directory may be renamed and moved again. Do not copy only `bin/`:
the facade, launcher, configuration, payload, and optional overrides form one
artifact. The destination filesystem must support ordinary Unix executable
files and symlinks.

Adding `<bundle>/bin` to a shell `PATH` is sufficient to expose all public
commands. Library and plugin variables should not be exported into the shell;
the launcher applies them separately to each bundled process.

## Portability boundary and current limits

HostBundle provides a verified route into a selected userspace, not universal
FHS virtualization. A compatible host currently means:

- Linux on the declared `x86_64` or `aarch64` architecture;
- a kernel at least as new as `min_kernel`;
- a CPU capable of running the payload's chosen instruction baseline;
- procfs mounted at `/proc`;
- access to any devices, sockets, services, and files required by the
  application.

The launcher checks architecture and kernel version. It does not probe the CPU
baseline or declaratively check `/dev/kvm`, DRM, Wayland, PipeWire, D-Bus, or
other application-specific host resources.

The process sees host absolute paths. Bundled glibc may read host `/etc`, NSS
configuration, timezone data, certificates, and other system state. In
particular, NSS setups involving host modules built for another glibc need
application-specific testing.

The build-time proof covers declared tools, script interpreters, and startup
ELF dependencies. It does not yet provide general audits for `dlopen`, Vulkan
ICD JSON, Mesa driver registries, QEMU firmware/modules, or arbitrary embedded
data and executable paths. Recipes must select those resources explicitly and
test the resulting application behavior.

The current verifier supports only `$ORIGIN` dynamic string expansion and does
not model `$LIB`, `$PLATFORM`, or glibc-hwcaps selection. `$LIB` and
`$PLATFORM` entries are rejected; glibc-hwcaps directories are not audited and
current bundles must not rely on them. `env -S`, an SDK profile with a broader
typed `PATH`, automatic export archives, internal hardlink preservation, and
additional filesystem metadata are also outside the current implementation.

These limits define the scope of the proof. Unsupported structured startup
constructs such as `$LIB` are rejected. Mechanisms outside the current startup
model, including later `dlopen` and glibc-hwcaps directory selection, are not
certified by a successful HostBundle build and require explicit recipe controls
and runtime tests.
