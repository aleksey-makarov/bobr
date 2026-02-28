# sysvinit-3.14 build error

- Attempts: 2 (latest with `localhost/mbuild-binary:bookworm-toolchain`)
- Result: failed
- Reason: compile phase runs, but generic recipe ends with `no supported build workflow for sysvinit-3.14`.
- Notes: needs package-specific install step/targets instead of generic fallback.
