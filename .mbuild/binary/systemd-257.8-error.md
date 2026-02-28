# systemd-257.8 build status

- recipe is migrated to `buildscript-meson`
- build fails in current container image because required tool `gperf` is missing

Observed failure:
- `meson.build:695:0: ERROR: Program 'gperf' not found or not executable`

What is needed:
- add `gperf` to the builder image
