# Remote repositories

A Bobr remote repository is a public, content-addressed cache of build and
reuse results. Its immutable objects can be served by an S3-compatible object
store or CDN, while a small signed master authenticates the current repository
state. Clients verify mappings, metadata, ordinary objects, and filesystem
files before importing them into a working store.

The repository protocol and its administration are documented in this
section:

- [`bobr-repo` command-line utility](CLI.md) describes preparation from a
  complete local store, signing on a separate trusted machine, publication,
  status reporting, and garbage collection.
- [Self-hosting with VersityGW](SERVER.md) shows how to run an S3-compatible
  endpoint, configure HTTPS and anonymous object reads, and generate the S3,
  TLS, and repository-signing credentials.
- [Repository master](MASTER.md) specifies the authenticated root, immutable
  mapping indexes and content lists, slots, retention, and the reader
  algorithm.
- [Ordinary objects](OBJECT.md) specifies the streaming envelope under `o/`.
- [Filesystem files](FS_FILE.md) specifies the streaming envelope under `f/`.
- [Directory tar profile](TAR.md) specifies the restricted canonical tar
  representation used for directory objects.

The normative CDDL schemas are:

- [`master-payload.cddl`](master-payload.cddl);
- [`object.cddl`](object.cddl);
- [`fs-file.cddl`](fs-file.cddl).

The `bobr-repo` Rust crate owns these formats and the mechanisms shared by the
Bobr client and repository administration utility.
