# Filesystem trees

A builder produces one object: an ordinary file or directory, or a **filesystem
tree** (fs-tree) — `bobr`'s content-addressed representation of a directory tree,
named by the hash of its manifest (see [Concepts](./CONCEPTS.md)). fs-trees are
the building blocks for whole filesystems: the root filesystem of a target
system, or the one a build runs inside (a `Sandbox` container).

A real filesystem needs files with specific owners and modes — `root`-owned
files, set-uid binaries — yet a build runs unprivileged and cannot `chown` to
`root`. So an fs-tree records each entry's **logical** owner and mode: the
ownership it should have in the finished filesystem, not that of the bytes as
`bobr` stores them. `bobr` applies it through a user namespace, building a
`root`-owned filesystem without ever being real root.

An fs-tree's payload is a **manifest** listing the tree's entries; regular-file
contents live separately as content-addressed **fs-files**, shared across trees.
Assembling a real directory from a manifest is fast and space-cheap — identical
files are hard-linked into place, sharing one inode — at the cost of one firm
rule: such a filesystem is mounted read-only, since writing a shared file would
change every tree that references it. An fs-tree is immutable.

The manifest lists one entry per path:

- a **directory** — with its logical `uid`, `gid`, and mode;
- a **symlink** — with its logical `uid`, `gid`, and target;
- a **regular file** — a reference to an fs-file.

A regular file's bytes, `uid`, `gid`, and mode belong to the fs-file it
references, identified by its own hash — not the [fsobj-hash](./FSOBJ_HASH.md)
that names objects; an fs-file hash covers one file and folds in its `uid`,
`gid`, and mode along with its bytes. Directory and symlink metadata live in the
manifest itself; its exact format is the [fs-tree Manifest](./FS_TREE_MANIFEST.md).

Most operations work on the manifest alone — merging or subsetting trees never
touches the files — and `bobr` materializes a real directory only when a builder
needs one. fs-trees model only regular files, directories, and symlinks; they
deliberately do not preserve inode topology (so `st_ino` and `st_nlink` are not
part of a tree's identity), nor xattrs, POSIX ACLs, or file capabilities (not
modeled yet).

Among the built-in builders:

- `FsTreeImport` turns an ordinary object into an fs-tree, and `OciExtract`
  produces one from an OCI image;
- `TreeMerge` and `TreeSubset` transform fs-trees at the manifest level, without
  materializing them;
- `ErofsRootfs`, `Initramfs`, and `Sandbox` consume a materialized fs-tree.

A recipe controls materialization by the **input name**: an input whose name
begins with `_` (e.g. `_rootfs`, `_tree`) is materialized into a real directory
before the builder runs; any other input is passed as the object itself.

See [Request](./REQUEST.md) for each builder.
