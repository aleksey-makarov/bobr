# Changelog

## 0.1.11

- Added the remote repository client with signed master verification, cached
  metadata, and verified object and fs-file downloads.
- Introduced request v6 with ordered, independent mapping and content providers
  for local and remote repositories, using shared asynchronous fetch scheduling.
- Improved repository administration and transfer progress, and fixed realtime
  terminal layout and activity sizing.
- Fixed hardlinked fs-file verification across user namespaces.
- Stopped shipping bundle launcher binaries; recipes now build the source crate.
- Moved the website and installer to `bobr.build` and separated installation
  instructions from Getting Started.

## 0.1.10

- Unified source acquisition and builder execution under a demand-driven,
  multi-goal Realizer with exact and reuse resolution.
- Added trusted and untrusted local repositories with hardlink and copy
  transports into the working store.
- Specified the signed remote repository format and added the `bobr-repo`
  publication and administration tooling. Realizer integration is not included
  yet.
- Added the public release installer, stable release asset names, PATH-based
  installation, and embedded build provenance.
