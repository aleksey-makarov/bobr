# systemd-257.8 build error

- Attempts: 2 (latest with `localhost/mbuild-binary:bookworm-toolchain`)
- Result: failed
- Reason: Meson configure fails: `Program 'gperf' not found or not executable`.
- Notes: add `gperf` to build image; systemd may require additional package-specific deps after that.
