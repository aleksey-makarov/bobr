# Design Index

## Recommended Reading Order

1. [TERM_MODEL.md](./TERM_MODEL.md)
   High-level execution model: one JSON DAG request, Rust-side planning, and
   bottom-up execution of missing nodes.

2. [IMAGE_BUILDERS.md](./IMAGE_BUILDERS.md)
   Current behavior of `Source/oci-registry`, `Image`, `OciExtract`, and
   `Sandbox` on the image and rootfs-backed execution paths.

3. [ROOTFS_BUILDERS.md](./ROOTFS_BUILDERS.md)
   Current behavior of `Tree`, `TreeSubset`, `TreeMerge`, `ErofsRootfs`,
   `Initramfs`, and the fs-tree filesystem authoring/composition path.

4. [SPLIT_OUTPUTS.md](./SPLIT_OUTPUTS.md)
   Naming convention for split package outputs and how those outputs are used
   in build and runtime dependency edges.

5. [CAS.md](./CAS.md)
   Content-addressed store, build handles, canonical result records, and
   publication refs.

6. [FSOBJ_HASH.md](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.
