# Contents

1. [Concepts](./CONCEPTS.md)
   The ideas bobr is built on — content addressing, objects, keys, and
   recipes — and how a build runs. The mental model behind the reference docs.

2. [Request](./REQUEST.md)
   The JSON request, the builder and source node shapes, higher-level
   recipe tags, and the CLI contract.

3. [Store](./STORE.md)
   Content-addressed store, build identity, canonical object records, build
   handles, and publication refs.

4. [OCI Image Inputs](./IMAGE_BUILDERS.md)
   Current behavior of `Source/OciRegistry`, `OciExtract`, and `Sandbox` on
   the OCI import and rootfs-backed execution paths.

5. [Rootfs Builders](./ROOTFS_BUILDERS.md)
   Current behavior of `Tree`, `TreeSubset`, `TreeMerge`, `ErofsRootfs`,
   `Initramfs`, and the fs-tree filesystem authoring/composition path.

6. [Split Outputs](./SPLIT_OUTPUTS.md)
   Naming convention for split package outputs and how those outputs are used
   in build and runtime dependency edges.

7. [Build logging](./LOGGING.md)
   Logging channels, store-log layout, the structured event record, the closed
   `status` vocabulary, and the format guarantees.

8. [Filesystem Object Hashing](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.

9. [fs-tree Manifest](./FS_TREE_MANIFEST.md)
   Canonical manifest format for manifest-addressed fs-tree artifacts.
