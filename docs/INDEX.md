# Design Index

## Recommended Reading Order

1. [TERM_MODEL.md](./TERM_MODEL.md)
   High-level execution model: one JSON DAG request, Rust-side planning, and
   bottom-up execution of missing nodes.

2. [IMAGE_BUILDERS.md](./IMAGE_BUILDERS.md)
   Current behavior of `Source/oci-registry`, `Image`, `Binary`, `Container`,
   and `Sandbox` on the image and rootfs-backed execution paths.

3. [ROOTFS_BUILDERS.md](./ROOTFS_BUILDERS.md)
   Current behavior of `Tree`, `TreeMerge`, `ErofsRootfs`, `Ext4Rootfs`, and
   the direct filesystem authoring/composition path.

4. [CAS.md](./CAS.md)
   Content-addressed store, build handles, canonical result records, and
   publication refs.

5. [FSOBJ_HASH.md](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.
