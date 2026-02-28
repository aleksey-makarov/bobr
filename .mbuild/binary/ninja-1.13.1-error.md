# ninja-1.13.1 build error

- Attempts: 2 (latest with `localhost/mbuild-binary:bookworm-toolchain`)
- Result: failed
- Reason: CMake step tries to fetch googletest from GitHub, but container networking is disabled (`--network=none`), so download fails.
- Notes: requires vendored dependency path / patch to avoid network fetch / explicit local test dependency.
