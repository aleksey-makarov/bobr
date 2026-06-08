# Store and fs-tree Awareness

This document summarizes how current builders and source origins interact with
store objects, fs-tree manifests, logical ownership, and extended ids.

## Builders

| Builder | Store object shape | fs-tree / extended-id behavior | Backend status |
| --- | --- | --- | --- |
| `Text` | One regular file object | Not fs-tree aware. The object has file bytes and executable state, but no manifest, idmap, logical uid, or logical gid. | Direct host-side file staging. |
| `Group` | One completion marker object | Not fs-tree aware. It does not inspect or rewrite filesystem ownership. | Metadata-only grouping. |
| `Tree` | Either one regular file object or one fs-tree directory object with `manifest.jsonl` and `root/` | File output is not fs-tree aware. Directory output writes an fs-tree manifest from `install` metadata and materializes ownership through the runtime helper. Authored ownership is limited to logical ids that fit the configured runtime idmap. | Uses `mbuild-runtime-helper` through the local helper path for directory ownership materialization. |
| `TreeSubset` | One fs-tree directory object | Reads the input manifest as the source of truth, selects manifest entries, preserves logical uid/gid/mode metadata, and materializes the selected tree in the ownership namespace. Regular files are hardlinked; copying is not allowed. | Uses the runtime helper for complete fs-tree object materialization. |
| `TreeMerge` | One fs-tree directory object | Reads input manifests, validates each input root against its manifest, merges logical uid/gid/mode metadata, and materializes the merged tree in the ownership namespace. Regular files are hardlinked; copying is not allowed. | Uses the runtime helper for complete fs-tree object materialization. |
| `ErofsRootfs` | One EROFS image file | Consumes fs-tree manifests. Tar headers used as the EROFS input carry logical uid, gid, mode, symlink targets, and deterministic mtimes from the merged manifest. | Uses the runtime helper to write the tar stream in the ownership namespace, then runs `mkfs.erofs` on the host. |
| `Initramfs` | One Linux `newc` cpio archive file | Consumes fs-tree manifests. Cpio headers carry logical uid, gid, mode, symlink targets, and deterministic mtimes from the merged manifest. | Uses the runtime helper to read input file bytes in the ownership namespace and write the archive. |
| `Sandbox` | One fs-tree directory object | Reads a prepared fs-tree rootfs as input and scans the produced output manifest after execution. Extended ids are handled by the sandbox launcher user namespace. | Uses the dedicated `mbuild-sandbox-runner` launcher path. |
| `OciExtract` | One fs-tree directory object plus `oci-config.json` | Reads uid/gid/mode information from OCI layer tar headers into the fs-tree manifest, including non-host logical ids, then materializes the extracted root. | Uses the runtime helper for ownership materialization where host ownership must be applied. |

`Sandbox` runs through a dedicated launcher because it needs step execution,
mount isolation, and output scanning inside one prepared root filesystem.
The parent-side fs-tree authoring and archive paths continue to use the local
runtime helper operations.

## Source and Origins

`Source` is store-aware but not fs-tree aware. A source origin materializes a
plain file or directory into a temporary staging path. The source executor
hashes that path, imports it into the content-addressed store, and checks the
imported object hash against the declared `object_hash`.

| Source/origin | Materialized path | Store behavior | fs-tree / extended-id behavior |
| --- | --- | --- | --- |
| `Source` without `origin` | Nothing is materialized. The declared object is expected to already exist in the store. | Reuses the existing `objects/<object_hash>` path and records the source object. | Not fs-tree aware. |
| `Path`, `unpack = false` | A local file or directory copied from an absolute host path into staging. | Imports the staged file or directory as a plain path object. | Does not read or write fs-tree manifests, idmaps, or logical ownership. |
| `Path`, `unpack = true` | A local tar archive unpacked into a staging directory. | Imports the unpacked directory as a plain path object. | Tar uid/gid metadata is not converted into fs-tree logical ids. |
| `Http`, `unpack = false` | A downloaded blob file. | Imports the downloaded file as a plain path object. | Not fs-tree aware. |
| `Http`, `unpack = true` | A downloaded archive unpacked into a staging directory. | Imports the unpacked directory as a plain path object. | Archive ownership metadata is not converted into fs-tree logical ids. |
| `OciRegistry` | An OCI image layout directory fetched by pinned digest. | Imports the OCI layout directory as a plain path object. | The origin itself is not fs-tree aware. `OciExtract` is the later builder that converts OCI layer metadata into an fs-tree manifest. |

If a source directory happens to contain files named `manifest.jsonl` and
`root/`, the source executor still treats it as an ordinary directory object.
Only builders that explicitly consume fs-tree inputs assign those names their
fs-tree meaning.
