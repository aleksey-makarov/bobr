# openssl-3.5.2 build status

- recipe is still inline (not migrated to buildscript)
- source tree does not match current generic scripts at top-level (`Configure`-based OpenSSL flow)

Why blocked now:
- current buildscript set does not include OpenSSL-specific configure/build/install logic

What is needed:
- add a dedicated OpenSSL build-script artifact (uses OpenSSL `Configure` flow)
