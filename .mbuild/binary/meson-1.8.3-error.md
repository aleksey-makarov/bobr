# meson-1.8.3 build error

- Attempts: 2 (latest with `localhost/mbuild-binary:bookworm-toolchain`)
- Result: failed
- Reason: generic recipe ended with `no supported build workflow for meson-1.8.3`.
- Notes: meson itself is a Python package; needs a Python-specific build/install recipe.
