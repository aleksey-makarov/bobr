# Design Index

## Recommended Reading Order

1. [JSON Graph Build Model](./TERM_MODEL.md)
   High-level execution model: one JSON DAG request, Rust-side planning, and
   bottom-up execution of missing nodes.

2. [OCI Image Inputs](./IMAGE_BUILDERS.md)
   Current behavior of `Source/oci-registry`, `OciExtract`, and `Sandbox` on
   the OCI import and rootfs-backed execution paths.

3. [Rootfs Builders](./ROOTFS_BUILDERS.md)
   Current behavior of `Tree`, `TreeSubset`, `TreeMerge`, `ErofsRootfs`,
   `Initramfs`, and the fs-tree filesystem authoring/composition path.

4. [Split Outputs](./SPLIT_OUTPUTS.md)
   Naming convention for split package outputs and how those outputs are used
   in build and runtime dependency edges.

5. [Content-Addressed Store](./CAS.md)
   Content-addressed store, build handles, canonical result records, and
   publication refs.

6. [Filesystem Object Hashing](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.

7. [Store and fs-tree Awareness](./STORE_AND_FS_TREE_AWARENESS.md)
   Current behavior of builders and source origins with store objects,
   fs-tree manifests, logical ownership, and extended ids.
