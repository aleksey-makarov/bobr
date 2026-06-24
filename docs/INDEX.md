# Design Index

## Recommended Reading Order

1. [JSON Graph Build Model](./TERM_MODEL.md)
   High-level execution model: one JSON DAG request, Rust-side planning, and
   bottom-up execution of missing nodes.

2. [OCI Image Inputs](./IMAGE_BUILDERS.md)
   Current behavior of `Source/OciRegistry`, `OciExtract`, and `Sandbox` on
   the OCI import and rootfs-backed execution paths.

3. [Rootfs Builders](./ROOTFS_BUILDERS.md)
   Current behavior of `Tree`, `TreeSubset`, `TreeMerge`, `ErofsRootfs`,
   `Initramfs`, and the fs-tree filesystem authoring/composition path.

4. [Split Outputs](./SPLIT_OUTPUTS.md)
   Naming convention for split package outputs and how those outputs are used
   in build and runtime dependency edges.

5. [Store](./STORE.md)
   Content-addressed store, build identity, canonical object records, build
   handles, and publication refs.

6. [Build logging](./LOGGING.md)
   Logging channels, store-log layout, the structured event record, the closed
   `status` vocabulary, and the format guarantees.

7. [Filesystem Object Hashing](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.

8. [fs-tree Manifest](./FS_TREE_MANIFEST.md)
   Canonical manifest format for manifest-addressed fs-tree artifacts.
