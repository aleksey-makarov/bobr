# Concepts

The result of a `bobr` build is an **object**: an immutable payload — a file or a
directory. Its identity is its `ObjectHash`, the hash of its content, computed by
[fsobj-hash](./FSOBJ_HASH.md).

Each object is produced by a **builder** — a component inside `bobr` — according
to a **recipe** describing how to build it. There are two kinds of builder:

- The `Source` builder uses no inputs: the object's content is fixed up front, so
  the recipe declares the `ObjectHash` to produce and, optionally, an **origin**
  saying how `bobr` can obtain it when configured stores do not have it.
- Every other builder (`Tree`, `Sandbox`, `Group`, …) builds its object from the
  recipe's **inputs**: the objects of other recipes it depends on.

Every recipe has a **`BuildKey`** — its identity. For the `Source` builder it is
just the declared `ObjectHash`; for any other builder it is computed from the
part of the recipe that says how to build the object from its inputs, together
with the `BuildKey`s of those inputs — that is, from everything in the recipe
that determines its result.

The recipes to build, together with the recipes they depend on, form a DAG.
`bobr` takes a JSON document — the **request** — that describes this graph,
realizes its goals, stores their objects in the **working store**, and
prints their `ObjectHash` values.

To realize a `Source`, bobr first checks whether its declared object is already
available from the working store or a configured local content source; if so,
there is nothing to fetch. Otherwise it fetches the content — from a local path,
an HTTP URL, or an OCI registry — computes its `ObjectHash`, and stores it. It
then checks that hash against the one the recipe declared. If they match, the
source is realized; if not, the fetched object still stays in the store (under
its real `ObjectHash`), but the source fails — it did not produce the object it
promised.

Builder reuse runs on two store mappings: `BuildKey` → `ObjectHash` and
`ReuseKey` → `ObjectHash`. For any other builder, bobr first looks up its
`BuildKey` in the working store and configured trusted secondary indexes. A hit
means that exact recipe was already built, so its complete object is reused and
everything below it is skipped.

On an exact miss, bobr resolves the possible object identities of the inputs and
computes the corresponding **`ReuseKey`** values from the same build
instructions plus those `ObjectHash`es. An input identity may come from a
mapping without its content having been copied into the working store yet. A
reuse hit — even from a different graph that reached the same input objects — is
then imported when necessary and reused. Only when exact and reuse resolution
both miss does bobr realize complete inputs locally, run the builder, store its
output, and add the mappings.

## Glossary

**builder** — A named component inside `bobr` that produces a recipe's object.
The `Source` builder acquires a pinned object; the others (`Tree`, `Sandbox`,
`Group`, …) build an object from the recipe's inputs.

**`BuildKey`** — A recipe's identity. For the `Source` builder it is the
`ObjectHash`; for any other builder it is computed from the part of the recipe
that says how to build the object from its inputs, together with the `BuildKey`s
of those inputs.

**input** — A named dependency of a recipe on another recipe, whose object the
builder consumes when it builds.

**object** — An immutable payload — a file or a directory — that `bobr` produces
and stores, named by its `ObjectHash`.

**`ObjectHash`** — An object's identity: a 64-character lowercase hex string that
names it by the hash of its content. Computed by [fsobj-hash](./FSOBJ_HASH.md).

**origin** — A named component inside `bobr` that the `Source` builder uses to
obtain its object: `Path`, `Http`, or `OciRegistry`.

**recipe** — A description of how to build one object, naming the **builder**
that builds it.

**request** — The JSON document `bobr` takes as input; it describes a recipe DAG
and its goals. See [Request](./REQUEST.md).

**`ReuseKey`** — A content-based identity, used when a recipe has inputs: like
its `BuildKey`, but computed from the `ObjectHash`es of those inputs instead of
their `BuildKey`s. It lets builds that reach the same input objects share one
stored object, even across different graphs.

**store** — The content-addressed store where `bobr` keeps objects, along with
the mappings `BuildKey` → `ObjectHash` and `ReuseKey` → `ObjectHash`. The
working store receives every result; configured local secondary stores may
provide trusted mappings and object content. See [Store](./STORE.md).
