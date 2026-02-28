# openssl-3.5.2 build error

- Attempts: 2 (latest with `localhost/mbuild-binary:bookworm-toolchain`)
- Result: failed
- Reason: generic recipe ended with `no supported build workflow for openssl-3.5.2`.
- Notes: OpenSSL needs package-specific build flow (`./Configure` + `make` + install targets), not generic CMake/Meson/Autotools fallback.
