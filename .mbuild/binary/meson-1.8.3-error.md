# meson-1.8.3 build status

- recipe is still inline (not migrated to buildscript)
- source tree is not a standard cmake/autotools project at top-level for current generic scripts

Why blocked now:
- current buildscript set (`gnu-make`, `cmake`, `meson`, `autotools`) is aimed at C/C++ style builds
- Meson itself needs a dedicated packaging/install flow (Python-oriented)

What is needed:
- add a dedicated build-script artifact for Meson project installation
