# Design Index

## Recommended Reading Order

1. [TERM_MODEL.md](./TERM_MODEL.md)  
   Start here for the high-level execution model. It explains the core architecture: Nickel defines pure builder terms, and Rust interprets one selected closed artifact term recursively.

2. [NICKEL_API.md](./NICKEL_API.md)  
   Read this next for the intended user-facing Nickel abstraction layer: artifacts, builder operations, bundles, package sets, and the boundary between Nickel terms and runtime internals.

3. [NICKEL_SKETCH.md](./NICKEL_SKETCH.md)  
   This is the concrete pseudo-Nickel companion to the API document. It shows how the proposed model might look in practice with builders, bundles, and package composition.

4. [FSOBJ_HASH.md](./FSOBJ_HASH.md)  
   Read this before the CAS details if you want the concrete object hashing rules. It defines the standalone structural hashing model shared by filesystem paths and tar archives.

5. [CAS.md](./CAS.md)  
   Read this after the term model is clear. It defines the content-addressed store, object and artifact identity, refs, and builder-specific runtime state boundaries.

6. [OVERRIDE_MODEL.md](./OVERRIDE_MODEL.md)  
   Read this last as a forward-looking extension. It describes how Nix-like override support can live in the Nickel layer without changing the core interpreter/CAS split.
