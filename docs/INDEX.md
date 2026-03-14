# Design Index

## Recommended Reading Order

1. [TERM_MODEL.md](./TERM_MODEL.md)  
   High-level execution model: Nickel defines pure builder terms, and Rust interprets one evaluated build request.

2. [NICKEL_API.md](./NICKEL_API.md)  
   User-facing Nickel abstraction layer: objects, builder operations, bundles, package sets, and build requests.

3. [NICKEL_SKETCH.md](./NICKEL_SKETCH.md)  
   Concrete Nickel examples for builders, bundles, package composition, and build requests.

4. [FSOBJ_HASH.md](./FSOBJ_HASH.md)  
   Structural hashing rules shared by filesystem paths and tar archives.

5. [CAS.md](./CAS.md)  
   Content-addressed object store, build records, refs, and builder-specific runtime state boundaries.

6. [OVERRIDE_MODEL.md](./OVERRIDE_MODEL.md)  
   Override semantics in the Nickel layer and their interaction with package construction, build keys, and build requests.
