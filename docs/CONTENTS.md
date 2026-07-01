# Contents

1. [Concepts](./CONCEPTS.md)
   The ideas bobr is built on — content addressing, objects, keys, and
   recipes — and how a build runs. The mental model behind the reference docs.

2. [Filesystem trees](./FS_TREE.md)
   How bobr represents filesystem trees as content-addressed objects:
   manifests, shared files, and materialization.

3. [Request](./REQUEST.md)
   The request format: the source and builder recipe shapes, the builders, and
   the source origins.

4. [Recipes in Nickel](./NICKEL.md)
   Authoring recipes in Nickel instead of raw JSON: the package set, overlays,
   build/runtime dependencies, and synthetic builders that expand into a request.

5. [Store](./STORE.md)
   Content-addressed store, build identity, canonical object records, reuse
   mappings, and name refs.

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
