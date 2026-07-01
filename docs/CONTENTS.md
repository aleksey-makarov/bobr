# Contents

1. [Concepts](./CONCEPTS.md)
   The ideas bobr is built on — content addressing, objects, keys, and
   recipes — and how a build runs. The mental model behind the reference docs.

2. [Request](./REQUEST.md)
   The request format: the envelope, the source and builder node shapes, the
   built-in builders, and the source origins.

3. [Recipes in Nickel](./NICKEL.md)
   How Nickel recipes lower to the JSON request (work in progress).

4. [Store](./STORE.md)
   Content-addressed store, build identity, canonical object records, build
   handles, and publication refs.

5. [Split Outputs](./SPLIT_OUTPUTS.md)
   Naming convention for split package outputs and how those outputs are used
   in build and runtime dependency edges.

6. [Build logging](./LOGGING.md)
   Logging channels, store-log layout, the structured event record, the closed
   `status` vocabulary, and the format guarantees.

7. [Filesystem Object Hashing](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.

8. [fs-tree Manifest](./FS_TREE_MANIFEST.md)
   Canonical manifest format for manifest-addressed fs-tree artifacts.
