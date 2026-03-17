# Design Index

## Recommended Reading Order

1. [TERM_MODEL.md](./TERM_MODEL.md)
   High-level execution model: one Nickel entry file evaluates to a STORE
   action tree, and Rust interprets that tree step by step.

2. [NICKEL_API.md](./NICKEL_API.md)
   User-facing Nickel abstraction layer: `return`, `bind`, primitive builder
   helpers, and realized `Build` values.

3. [NICKEL_SKETCH.md](./NICKEL_SKETCH.md)
   Concrete Nickel examples for self-contained recipes, monadic dependency
   sequencing, and reading builder-generated metadata.

4. [FSOBJ_HASH.md](./FSOBJ_HASH.md)
   Structural hashing rules shared by filesystem paths and tar archives.

5. [CAS.md](./CAS.md)
   Content-addressed object store, build records, publication refs, and the
   Rust-side interpreter/store responsibilities.

6. [OVERRIDE_MODEL.md](./OVERRIDE_MODEL.md)
   Override semantics in the Nickel layer and their interaction with payloads,
   dependency sequencing, and publication names.
