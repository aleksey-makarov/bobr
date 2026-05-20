# Split Outputs

## Summary

Package recipes may expose split outputs for narrower dependency edges. The
unsuffixed package attribute remains the full install tree, while suffixed
attributes describe content groups that can be used in `deps.runtime` and
`deps.build`.

For example:

- `pkgs.glibc` is the full `glibc` install tree
- `pkgs.glibc_libs` contains the runtime library subset
- `pkgs.glibc_tools` contains command-line tools
- `pkgs.glibc_dev` contains development files

Nickel attributes use `snake_case`. Builder names use `kebab-case` and keep the
package version, for example `glibc-libs-2.42`.

## Standard Suffixes

- `_libs`: runtime shared libraries, the dynamic loader, SONAME symlinks, and
  minimal files needed to load those libraries.
- `_tools`: executable utilities shipped by the package.
- `_dev`: headers, pkg-config files, CMake metadata, linker scripts, and
  unversioned linker `.so` symlinks.
- `_static`: static libraries and static-only artifacts.
- `_doc`: documentation, man/info pages, and examples.
- `_locale`: gettext catalogs, locale data, and translations.
- `_data`: other architecture-independent runtime data that is not documentation
  or locale data.

Avoid `_runtime`, because runtime is already the dependency role expressed by
`deps.runtime`. Avoid `_closure`, because `RootfsClosure` and
`*-rootfs-closure` already use that term.

Existing `_src`, `_gen1`, and bootstrap attributes keep their current meaning.

## Dependency Rules

Recipes should depend on the narrowest split output that provides the required
files.

- `deps.runtime` should usually point at `_libs` outputs for library
  dependencies. For example, packages that only need libc should depend on
  `pkgs.glibc_libs`, not the full `pkgs.glibc`.
- `deps.build` should use `_dev` when headers or build metadata are needed, and
  `_tools` when executable tools are needed.
- The full `pkgs.foo` output should be used only when the whole install tree is
  required.

Split outputs declare their own runtime dependencies. For example,
`glibc_tools` depends at runtime on `glibc_libs`, and a future `gmp_dev` output
would depend at runtime on `gmp_libs`.
