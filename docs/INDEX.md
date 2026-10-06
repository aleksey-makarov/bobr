{{#include ../README.md}}

## Documentation

1. [Installation](./INSTALLATION.md)
   Host requirements, release installation, PATH setup, and uninstallation.

2. [Getting Started](./GETTING_STARTED.md)
   Build your first object from a tiny hand-written request, then a real
   target built from the Nickel recipes, and how to change a recipe without
   editing it.

3. [Concepts](./CONCEPTS.md)
   The ideas bobr is built on — content addressing, objects, keys, and
   recipes — and how a build runs. The mental model behind the reference docs.

4. [Filesystem trees](./FS_TREE.md)
   How bobr represents filesystem trees as content-addressed objects:
   manifests, shared files, and materialization.

5. [Request](./REQUEST.md)
   The request format: the source and builder recipe shapes, the builders, and
   the source origins.

6. [Recipes in Nickel](./NICKEL.md)
   Authoring recipes in Nickel instead of raw JSON: the package set, overlays,
   build/runtime dependencies, split outputs, and synthetic builders that expand
   into a request.

7. [HostBundle](./HOST_BUNDLE.md)
   Building verified, relocatable host-side application directories: payloads,
   launchers, wrappers, typed environment, startup verification, and runtime
   behavior.

8. [Store](./STORE.md)
   Content-addressed store, build identity, canonical object records, reuse
   mappings, and name refs.

9. [Remote repositories](./REMOTE_REPOSITORY/OVERVIEW.md)
   Publishing complete stores as authenticated content-addressed caches:
   signed masters, immutable indexes and content, slot retention, the
   `bobr-repo` administration workflow, and garbage collection.

   - [`bobr-repo` command-line utility](./REMOTE_REPOSITORY/CLI.md)
   - [Self-hosting with VersityGW](./REMOTE_REPOSITORY/SERVER.md)
   - [Repository master](./REMOTE_REPOSITORY/MASTER.md)
   - [Ordinary objects](./REMOTE_REPOSITORY/OBJECT.md)
   - [Filesystem files](./REMOTE_REPOSITORY/FS_FILE.md)
   - [Directory tar profile](./REMOTE_REPOSITORY/TAR.md)

10. [Build logging](./LOGGING.md)
   Logging channels, store-log layout, the structured event record, the closed
   `status` vocabulary, and the format guarantees.

11. [Filesystem Object Hashing](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.

12. [fs-tree Manifest](./FS_TREE_MANIFEST.md)
   Canonical manifest format for manifest-addressed fs-tree artifacts.

13. [Development](./DEVELOPMENT.md)
   Working on bobr rather than with it: building from a source checkout, and
   rebuilding the world into a fresh store.

14. [Scheduler](./SCHEDULER.md)
   Demand-driven DAG realization, exact and reuse resolution, Source
    acquisition, builder execution, concurrency limits, and cancellation.
