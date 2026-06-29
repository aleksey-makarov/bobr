# Overview

These documents describe bobr's design and internals.

## Recommended Reading Order

1. [Concepts](./CONCEPTS.md)
   The ideas bobr is built on — content addressing, objects, keys, and
   recipes — and how a build runs. The mental model behind the reference docs.

2. [JSON Graph Build Model](./TERM_MODEL.md)
   High-level execution model: one JSON DAG request, Rust-side planning, and
   bottom-up execution of missing nodes.

3. [Request and store format](./REQUEST_FORMAT.md)
   The JSON request envelope, node shapes, the content-addressed store layout,
   build/reuse keys, and the CLI contract.

4. [OCI Image Inputs](./IMAGE_BUILDERS.md)
   Current behavior of `Source/OciRegistry`, `OciExtract`, and `Sandbox` on
   the OCI import and rootfs-backed execution paths.

5. [Rootfs Builders](./ROOTFS_BUILDERS.md)
   Current behavior of `Tree`, `TreeSubset`, `TreeMerge`, `ErofsRootfs`,
   `Initramfs`, and the fs-tree filesystem authoring/composition path.

6. [Split Outputs](./SPLIT_OUTPUTS.md)
   Naming convention for split package outputs and how those outputs are used
   in build and runtime dependency edges.

7. [Store](./STORE.md)
   Content-addressed store, build identity, canonical object records, build
   handles, and publication refs.

8. [Build logging](./LOGGING.md)
   Logging channels, store-log layout, the structured event record, the closed
   `status` vocabulary, and the format guarantees.

9. [Filesystem Object Hashing](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.

10. [fs-tree Manifest](./FS_TREE_MANIFEST.md)
    Canonical manifest format for manifest-addressed fs-tree artifacts.
