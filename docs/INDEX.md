# Design Index

## Recommended Reading Order

1. [TERM_MODEL.md](./TERM_MODEL.md)
   High-level execution model: one JSON recipe graph, Rust-side planning, and
   bottom-up execution of missing nodes.

2. [IMAGE_BUILDERS.md](./IMAGE_BUILDERS.md)
   Current behavior of `ContainerImage`, `Image`, and `Binary` on the image
   path.

3. [CAS.md](./CAS.md)
   Content-addressed store, build handles, canonical result records, and
   publication refs.

4. [FSOBJ_HASH.md](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.
