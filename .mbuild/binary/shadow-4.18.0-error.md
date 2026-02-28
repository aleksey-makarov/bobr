# shadow-4.18.0 build error

- Attempts: 2 (latest with `localhost/mbuild-binary:bookworm-toolchain`)
- Result: failed
- Reason: `autoreconf` fails because `autopoint` is missing in the current image.
- Notes: add `autopoint` (gettext tooling) to build image, then retry.
