# Store and fs-tree Awareness

This document summarizes how current builders and source origins interact with
store objects, fs-tree manifests, logical ownership, and extended ids.

## Builders

| Builder | Store object shape | fs-tree / extended-id behavior | Backend status |
| --- | --- | --- | --- |
| `Text` | One regular file object | Not fs-tree aware. The object has file bytes and executable state, but no manifest, idmap, logical uid, or logical gid. | Direct host-side file staging. |
| `Group` | One completion marker object | Not fs-tree aware. It does not inspect or rewrite filesystem ownership. | Metadata-only grouping. |
| `Tree` | One regular file object or one ordinary directory object | Not fs-tree aware. It stages exactly the file or directory described by its `tree` config. | Direct host-side staging. |
| `FsTreeImport` | One fs-tree v2 manifest object | Reads one staged input tree plus install rules, imports file payloads into `fs-files`, and writes a canonical fs-tree v2 manifest object. | Uses a `bobr-runtime` namespace function because import may need root-visible ownership metadata. |
| `TreeSubset` | One fs-tree v2 manifest object | Reads the input manifest as the source of truth, selects manifest entries, and preserves logical uid/gid/mode metadata. | Manifest-only operation. |
| `TreeMerge` | One fs-tree v2 manifest object | Reads input manifests, validates overlapping entries, and merges logical uid/gid/mode metadata. | Manifest-only operation. |
| `ErofsRootfs` | One EROFS image file | Consumes one fs-tree input materialized as a filesystem root by the runtime before builder execution. | Uses a `bobr-runtime` namespace function to run `mkfs.erofs` on the materialized root. |
| `Initramfs` | One Linux `newc` cpio archive file | Consumes one fs-tree input materialized as a filesystem root by the runtime before builder execution. | Uses a `bobr-runtime` namespace function to read the materialized root and write the archive. |
| `Sandbox` | One fs-tree v2 manifest object | Reads a prepared fs-tree rootfs as input and scans the produced output manifest after execution. Extended ids are handled by the sandbox launcher user namespace. | Uses a `bobr-runtime` function plus the dedicated `mbuild-sandbox-runner` launcher path. |
| `OciExtract` | One fs-tree v2 manifest object | Reads uid/gid/mode information from OCI layer tar headers into the fs-tree manifest, including non-host logical ids. | Uses a `bobr-runtime` namespace function to extract layers and import the resulting tree into fs-tree v2 storage. |

`Sandbox` runs through a dedicated launcher because it needs step execution,
mount isolation, and output scanning inside one prepared root filesystem. Other
operations that need namespace-root filesystem access run through typed
`bobr-runtime` functions.

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
