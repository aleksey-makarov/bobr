# Design Index

## Recommended Reading Order

1. [TERM_MODEL.md](./TERM_MODEL.md)  
   High-level execution model: Rust embeds Nickel and interprets primitive STORE operations that produce `Built` values.

2. [NICKEL_API.md](./NICKEL_API.md)  
   User-facing Nickel abstraction layer: primitive builder calls, explicit publication names, and `Built` values.

3. [NICKEL_SKETCH.md](./NICKEL_SKETCH.md)  
   Concrete Nickel examples for package composition, primitive builder calls, and reading builder-generated metadata.

4. [FSOBJ_HASH.md](./FSOBJ_HASH.md)  
   Structural hashing rules shared by filesystem paths and tar archives.

5. [CAS.md](./CAS.md)  
   Content-addressed object store, build records, publication refs, and builder-specific runtime-state boundaries.

6. [OVERRIDE_MODEL.md](./OVERRIDE_MODEL.md)  
   Override semantics in the Nickel layer and their interaction with payloads, dependency selection, and publication names.
