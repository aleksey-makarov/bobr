# Concepts

The result of a `bobr` build is an **object**: an immutable payload — a file or a
directory. Its identity is its `ObjectHash`, the hash of its content, computed by
[fobj-hash](./FSOBJ_HASH.md).

Each object is produced by `bobr` according to a **recipe** — a description of how
to build one object. There are two kinds of recipe:

- A **source recipe** is a leaf: it has no inputs, so its content is fixed up
  front. It declares the `ObjectHash` of the object it must produce.
- A **builder recipe** describes how to produce its object from its **inputs**
  (the objects of other recipes it depends on).

Every recipe has a **`BuildKey`** — its identity. For a source recipe the
`BuildKey` is just its `ObjectHash`. For a builder recipe it is computed from the
part of the recipe that says how to build the object from its inputs, together
with the `BuildKey`s of those inputs — that is, from everything in the recipe
that determines its result.

The recipe to build, together with the recipes it depends on, forms a DAG. `bobr`
takes a JSON document — the **request** — that describes this graph, builds the
recipe, stores its object in the **store**, and prints that object's
`ObjectHash`.

To build a source recipe, `bobr` first checks whether the object it declares is
already in the store; if so, there is nothing to fetch. Otherwise it fetches the
content — from a local path, an HTTP URL, or an OCI registry — computes its
`ObjectHash`, and stores it. It then checks that hash against the one the recipe
declared. If they match, the source is built; if not, the fetched object still
stays in the store (under its real `ObjectHash`), but the source fails — it did
not produce the object it promised.

Builder reuse runs on two store mappings: `BuildKey` → `ObjectHash` and
`ReuseKey` → `ObjectHash`. To build a builder recipe, `bobr` first looks up its
`BuildKey` — a hit means that exact recipe was already built, so it reuses the
stored object and skips everything below. Otherwise it builds the inputs first
(each is itself a recipe), computes a **`ReuseKey`** from the same build
instructions plus the `ObjectHash`es of those inputs, and looks that up — a hit,
even from a different graph that reached the same inputs, is reused too. Only
when both miss does `bobr` run the builder, store the object, and add both
mappings.

<!-- 

 `bobr` uses the `BuildKey` to
recognize work it has already done and skip it.

    and, usually, an `origin`
  saying where to fetch it from (a local path, an HTTP URL, an OCI registry).
 The hash both names the source and verifies it after fetching.

 has a `tag` (which builder to run), an opaque `config`, and
  named `inputs` (its dependencies). The builder turns its config and resolved
  inputs into one object.


`bobr` turns a graph of recipes into content-addressed artifacts on Linux. You
describe what you want as a set of *recipe nodes*; `bobr` plans the graph, builds
only what is missing, and stores each result as an immutable object named by the
hash of its content.

Recipes are normally written declaratively in
[Nickel](https://nickel-lang.org/) and lowered to a single JSON request — a flat
DAG of nodes — which the engine executes. `bobr` itself only ever sees that JSON
request; it has no embedded recipe language. The exact request shape is the
[Request and store format](./REQUEST_FORMAT.md).


## Recipes and the request graph

A request is a directed acyclic graph (DAG) of nodes, each with a technical id.
Dependencies are id references, so one node can be shared by many parents. The
reserved id `root` is the target of the current build.

## Objects and the store


Alongside each object, `bobr` keeps a small **object record** capturing the
identities of the object's direct inputs. Human-facing **names** are mutable
*refs* layered on top — `object-refs/<name>` points at the latest object built
for that name — so names can move while objects stay immutable. The full on-disk
layout is in [Store](./STORE.md).

## Keys: build identity

`bobr` uses three distinct identities, and keeping them apart is the key to how it
caches and reuses work:

- **`object_hash`** — *content* identity. Names a finished object by what it
  contains.
- **`build_key`** — *planning* identity. Computed from a builder's tag, its
  normalized config, and the `build_key`s of its dependencies. It names a node
  in the graph before anything is built, so `bobr` can check whether a result
  already exists without first realizing the inputs. For a source node, the
  `build_key` is just its declared `object_hash`.
- **`reuse_key`** — *reuse* identity. Computed from a builder's tag, its config,
  and the `object_hash`es of its (now realized) dependencies. Because it depends
  on input *content* rather than on which graph produced it, two different
  graphs that reach the same inputs can share one stored object.

The exact computation and ordering rules are in
[Store](./STORE.md#identity-model).

## How a build runs

`bobr` plans top-down from `root` and builds bottom-up:

1. **Plan.** For each builder node, `bobr` first checks for a build handle on its
   `build_key`; failing that, a canonical object for its `reuse_key`; only if
   both miss does it recurse into the node's inputs. This finds the smallest set
   of nodes that actually need building.
2. **Build.** A node becomes ready once all its inputs are reused or built;
   ready nodes run in parallel in a worker pool, and no `build_key` is ever
   built twice.
3. **Sources** join the same flow: `bobr` looks for the object by `object_hash`,
   otherwise fetches it from its origin and verifies the hash before importing
   it.

The result of the whole request is the `root` object, published under its name.

-->

## Glossary

**`BuildKey`** — A recipe's identity. For a source recipe it is the `ObjectHash`;
for a builder recipe it is computed from the part of the recipe that says how to
build the object from its inputs, together with the `BuildKey`s of those inputs.

**input** — A named dependency of a builder recipe on another recipe, whose
object the builder consumes when it builds.

**object** — An immutable payload — a file or a directory — that `bobr` produces
and stores, named by its `ObjectHash`.

**`ObjectHash`** — An object's identity: a 64-character lowercase hex string that
names it by the hash of its content. Computed by [fobj-hash](./FSOBJ_HASH.md).

**recipe** — A description of how to build one object. A *source recipe* is a
leaf with no inputs; a *builder recipe* describes how to build its object from
its inputs.

**request** — The JSON document `bobr` takes as input; it describes the recipe DAG
to build. See [Request and store format](./REQUEST_FORMAT.md).

**`ReuseKey`** — A builder recipe's content-based identity: like its `BuildKey`,
but computed from the `ObjectHash`es of its inputs instead of their `BuildKey`s.
It lets builds that reach the same input objects share one stored object, even
across different graphs.

**store** — The content-addressed store where `bobr` keeps objects, along with
the mappings `BuildKey` → `ObjectHash` and `ReuseKey` → `ObjectHash`. See
[Store](./STORE.md).
